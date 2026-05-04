# Prov Dogfooding & End-to-End Testing Guide

A by-hand walkthrough of every shipped feature, with edge cases the test
suite has caught regressions on at least once. Use this when validating a
release candidate, after a refactor that touches a hot path, or when you
just want to feel the surface from a user's seat.

The capture flow is exercised two ways. Section 2 drives a **real Claude
Code session** end-to-end — the only way to prove that what the agent
actually emits matches what `prov hook ...` actually consumes. Section 3
drives the same hook handlers with hand-crafted JSON for everything you
can't easily reproduce in a real session (malformed payloads, stale
staging dirs, specific edge regexes). Run both — they're complementary,
not redundant.

The remaining stub (`prov backfill`) is listed at the end so you can
confirm it still fails loudly rather than silently no-op. (`prov
regenerate` was dropped from v1; see the plan's U14 status note.)

## 0. Prerequisites

```bash
# 1. From the prov repo, build a release binary and put it on PATH for the
#    sandbox shell. Do NOT install globally — tests should run against the
#    binary you just built.
cd /Users/matt/Documents/GitHub/prov
cargo build --release
export PATH="$PWD/target/release:$PATH"
prov --version       # confirms the right binary is on PATH

# 2. CI parity check before you start (optional but recommended after rebases).
./scripts/check.sh
```

Create a fresh sandbox repo so nothing pollutes your real working trees:

```bash
SANDBOX="$(mktemp -d /tmp/prov-dogfood-XXXX)"
cd "$SANDBOX"
git init -q
git commit --allow-empty -m "root"
echo "$SANDBOX"     # remember this path; cleanup at the end
```

For sync tests you also want a bare "remote" peer:

```bash
PEER="$(mktemp -d /tmp/prov-peer-XXXX)"
git init --bare -q "$PEER"
git -C "$SANDBOX" remote add origin "$PEER"
```

## 1. Install / uninstall

### Golden path

```bash
cd "$SANDBOX"
prov install
ls .git/hooks/                    # post-commit, pre-push, post-rewrite present
cat .claude/settings.json         # 4 prov hook entries (SessionStart, UserPromptSubmit, PostToolUse, Stop)
git config --get notes.displayRef         # refs/notes/prompts
git config --get notes.mergeStrategy      # manual
git config --get notes.rewrite.amend      # false
test -f .git/prov.db && echo "cache OK"
```

### Edge cases

```bash
# Idempotent re-install: same on-disk state, no duplicate hook blocks.
prov install
grep -c '# >>> prov' .git/hooks/post-commit          # exactly 1
grep -c '"command": "prov hook' .claude/settings.json # exactly 4

# Pre-existing user content in a hook: prov block is added/refreshed in place.
echo 'echo user-hook' >> .git/hooks/post-commit
prov install
grep -A1 '# <<< prov' .git/hooks/post-commit         # user content survives

# Team-mode opt-in (writes a fetch refspec, arms the pre-push gate).
prov install --enable-push origin
git config --get-all remote.origin.fetch             # includes refs/notes/prompts:refs/notes/origin/prompts

# Uninstall removes prov-managed lines but leaves your data.
prov uninstall
grep -c '# >>> prov' .git/hooks/post-commit          # 0
test -f .git/prov.db && echo "cache preserved (expected)"
git for-each-ref refs/notes/                          # notes ref untouched

# --purge also drops cache and staging.
prov install
prov uninstall --purge
test -f .git/prov.db || echo "cache gone"
test -d .git/prov-staging || echo "staging gone"

# Uninstall is idempotent.
prov uninstall --purge
prov uninstall --purge   # no error
```

Reinstall before continuing:

```bash
prov install --enable-push origin
```

### Plugin install (alternative path) — U11

`prov install` is the per-repo path. The Claude Code plugin shape (under
`plugin/` in this repo) is the alternative for a global install across
every repo where the `prov` binary is on `PATH`. The two paths register
the same four hooks; the plugin path doesn't write `.git/hooks/...` or
`.claude/settings.json` — Claude Code itself loads the plugin's
`hooks/hooks.json` directly.

```bash
# `prov install --plugin` is informational only — it prints the
# marketplace install command and exits without touching `.claude/`.
prov install --plugin
test -d "$SANDBOX/.claude" && echo "FAIL: --plugin should not create .claude/" \
  || echo "OK: --plugin did not mutate the project"

# Inspect the plugin manifest shipped with prov.
PLUGIN_DIR="$(dirname "$(command -v prov)")/../../plugin"  # adjust if your binary lives elsewhere
# When dogfooding from this repo:
PLUGIN_DIR="/Users/matt/Documents/GitHub/prov/plugin"
cat "$PLUGIN_DIR/.claude-plugin/plugin.json" | jq '.name, .version, .description'

# Confirm the four hook events are registered with the right matchers and timeouts.
jq '.hooks | keys' "$PLUGIN_DIR/hooks/hooks.json"
# Expect: ["PostToolUse","SessionStart","Stop","UserPromptSubmit"]
jq '.hooks.PostToolUse[0].matcher' "$PLUGIN_DIR/hooks/hooks.json"
# Expect: "Edit|Write|MultiEdit"
jq '.hooks.PostToolUse[0].hooks[0] | {type, command, timeout}' "$PLUGIN_DIR/hooks/hooks.json"
# Expect: {"type":"command","command":"prov hook post-tool-use","timeout":5}

# Local plugin install against a real Claude Code session — drop into a
# fresh repo (so the per-repo `.claude/settings.json` from earlier doesn't
# mask the plugin's hooks) and load the plugin from the local directory.
PLUGIN_SANDBOX="$(mktemp -d /tmp/prov-plugin-sandbox-XXXX)"
cd "$PLUGIN_SANDBOX"
git init -q
git commit --allow-empty -m "root"
# In Claude Code (separate terminal):
#   /plugin install --plugin-dir /Users/matt/Documents/GitHub/prov/plugin
# Then run a session with at least one Edit/Write tool use, exit, and:
git add . 2>/dev/null
git commit -qm "plugin install smoke" || true
ls .git/prov-staging/ 2>/dev/null && echo "OK: plugin path captured a session" \
  || echo "FAIL: no staging dir — hooks didn't fire"
cd "$SANDBOX"
rm -rf "$PLUGIN_SANDBOX"
```

The automated layout lints in `crates/prov-cli/tests/cli_plugin_layout.rs`
catch frontmatter regressions; the manual run above confirms the
marketplace install path actually carries a session through capture.

## 2. End-to-end with a real Claude Code session

This is the integration smoke test. You're verifying that Claude Code
actually invokes the four registered hooks with the JSON shapes
`prov hook ...` expects, and that the post-commit flush produces a
useful note. If anything in this section fails, the simulated tests in
section 3 will lie to you — the simulator and reality have drifted.

> **Restart Claude Code first.** `.claude/settings.json` is read at
> session start; if the project's settings file was just written by
> `prov install`, an already-running CLI won't see the new hook entries.

### 2.1 Capture, read, search (golden path)

In a second terminal, launch Claude Code in the sandbox and ask it to
do something with at least 2-3 turns and a non-trivial edit:

```bash
cd "$SANDBOX"
claude       # or however you launch Claude Code
```

A representative prompt sequence to type into the session:

> Turn 1: Create `src/greet.ts` exporting a `greet(name: string)`
> function that returns a friendly greeting. Add a brief test in
> `src/greet.test.ts`.
>
> Turn 2: Tighten the error path so an empty name throws a
> `TypeError` with a clear message.
>
> Turn 3: Extract the greeting template into a `templates.ts`
> module so it can be reused.

Exit Claude Code, then back in your dogfooding shell:

```bash
# Staging shape — every UserPromptSubmit lands a turn-N.json,
# every Edit/Write/MultiEdit appends to edits.jsonl.
ls .git/prov-staging/                          # one or more <session-id> dirs
ls .git/prov-staging/*/turn-*.json
ls .git/prov-staging/*/edits.jsonl

# Commit and confirm the post-commit hook flushes a real note onto HEAD.
git add . && git commit -qm "feat: greet"
git notes --ref=refs/notes/prompts list | head
git notes --ref=refs/notes/prompts show HEAD | jq '.edits | length'   # > 0
git notes --ref=refs/notes/prompts show HEAD | jq '.edits[0].prompt'  # the real prompt text

# Read surface against real captured turns.
prov log src/greet.ts
prov log src/greet.ts:1                        # point lookup against a real edit
prov log src/greet.ts --history                # earlier turns appear via derived_from
prov search "greet"                            # FTS hit on the real prompt
```

If `prov log` returns no provenance for a line you know was just edited,
inspect `.git/prov-staging/` — the staging files are the source of
truth for what the hook captured before the post-commit flush.

### 2.2 Private routing (real session)

Open a fresh Claude Code session and start the prompt with the opt-out
marker:

> ```
> # prov:private
>
> Add a placeholder `loadStagingCredentials()` to `src/secrets.ts`.
> ```

Make a commit, then verify routing landed it on the private ref only:

```bash
SID=$(ls -t .git/prov-staging | head -1)
ls .git/prov-staging/$SID/private/             # turn-1.json present here
ls .git/prov-staging/$SID/turn-*.json 2>/dev/null \
  || echo "nothing in public dir (expected)"

git add . && git commit -qm "feat: secrets stub"
git notes --ref=refs/notes/prompts show HEAD 2>/dev/null \
  || echo "no public note for this commit (expected)"
git notes --ref=refs/notes/prompts-private show HEAD | jq '.edits | length'

prov log src/secrets.ts                        # local read overlays private on public
prov push origin
git --git-dir="$PEER" for-each-ref refs/notes/prompts-private 2>/dev/null \
  || echo "private ref absent on remote (expected)"
```

### 2.3 Redaction (real session)

The redactor runs at write time, before staging hits disk. To exercise
the typed detectors, paste known-format secrets into the prompt itself:

> Use the AWS access key `AKIAIOSFODNN7EXAMPLE` and the GitHub token
> `ghp_1234567890abcdefghijABCDEFGHIJABCD` to wire up a placeholder
> credential provider in `src/creds.ts`.

(The values above are well-known test fixtures, not live credentials.)
Make a commit, then verify nothing leaked:

```bash
SID=$(ls -t .git/prov-staging | head -1)
grep REDACTED .git/prov-staging/$SID/turn-*.json
git add . && git commit -qm "feat: creds stub"
git notes --ref=refs/notes/prompts show HEAD | grep -c REDACTED   # >= 2
git notes --ref=refs/notes/prompts show HEAD | grep -E 'AKIA|ghp_' \
  && echo "FAIL: secret reached the note" \
  || echo "OK: redactor caught both"
```

### 2.4 Rewrite migration (real session + amend)

After capturing real turns above, exercise the post-rewrite hook with
an amend:

```bash
HEAD_BEFORE=$(git rev-parse HEAD)
echo "// tweak" >> src/greet.ts
git commit -qa --amend --no-edit
HEAD_AFTER=$(git rev-parse HEAD)
git notes --ref=refs/notes/prompts list | grep "$HEAD_AFTER" && echo "migrated"
git notes --ref=refs/notes/prompts list | grep "$HEAD_BEFORE" \
  || echo "old SHA cleaned up"
```

### 2.5 Skill — agent-side trigger fidelity (U12)

The Skill at `plugin/skills/prov/SKILL.md` teaches Claude Code to query
its own provenance before substantive edits. Trigger fidelity (does the
agent fire on the right asks, stay quiet on the wrong ones?) is what
this section verifies — there is no automated harness for that.

The four scenarios below are the load-bearing manual smoke. Run them
after any meaningful edit to the SKILL frontmatter `description:` or
body. The full test plan with iteration loop lives at
`plugin/skills/prov/tests/skill_smoke.md`.

**Setup once:** install the plugin (per the alternative-install section
above) so the Skill ships alongside the hooks. Confirm via:

```bash
# In Claude Code: /skills should list `prov` with the trigger-rich description.
# Or inspect on disk:
cat /Users/matt/Documents/GitHub/prov/plugin/skills/prov/SKILL.md | head -20
```

You also need a fixture repo with at least one captured prov note —
the sandbox from sections 2.1-2.4 already has this. Pick a real captured
prompt to use as the "load-bearing" one in scenario 1.

**Scenario 1 — substantive ask triggers the Skill.** In Claude Code,
ask:

> Refactor `src/greet.ts` to extract the greeting template into a
> separate function.

Pass criteria:
- Agent calls `prov log src/greet.ts` (or `:<line>`) before proposing
  edits.
- Agent's plan cites the captured prompt verbatim and treats the prior
  constraint as load-bearing.

**Scenario 2 — trivial single-line change does NOT trigger.** Ask:

> Fix the typo on line 12 of `README.md`.

Pass criteria: no `prov log` invocation. The `paths:` glob excludes
`*.md`, so the Skill should not even surface.

**Scenario 3 — greenfield does NOT trigger.** Ask:

> Create a new file `src/utils/format.ts` with a date formatter that
> outputs `YYYY-MM-DD`.

Pass criteria: either no `prov log` call, or one that returns empty
(via `--only-if-substantial`) and the agent proceeds without surfacing
it.

**Scenario 4 — drifted line surfaces drift state.** First, hand-edit
a line that was originally AI-written:

```bash
# Pick a line `prov log` reports as `unchanged`, edit it, commit.
sed -i '' 's/friendly/warm/' src/greet.ts
git commit -qam "human tweak"
prov log src/greet.ts:1                      # expect status: drifted
```

Then ask Claude Code:

> Explain `src/greet.ts:1`.

Pass criteria: agent runs `prov log src/greet.ts:1`, surfaces both the
original prompt AND the drift state ("hand-edited after the original AI
write"), and frames its explanation around both.

**If a scenario fails:** iterate on the `description:` field (false
negatives → add trigger phrasing; false positives → strengthen the
"When NOT to use it" body section). Re-run all four after each
iteration; the trigger surface is global, so a tweak that fixes
scenario 1 may regress scenario 2.

The same content lints automated in `cli_skill_layout.rs` (frontmatter
present, body ≤500 lines, references linked) catch regressions in the
SKILL artifact itself; what they cannot catch is whether the agent
actually fires correctly. That's why this section is manual.

### 2.6 What "working end-to-end" means here

By the end of section 2 you should have observed all of:

- Real Claude Code prompts staged into `.git/prov-staging/<session-id>/`
- A `post-commit` hook flushed those into a note attached to HEAD
- `prov log` and `prov search` returned the real prompt text on demand
- `# prov:private` routed an entire session away from the public ref,
  and `prov push` did not push the private ref
- The redactor caught known-format secrets *before* they hit staging
- `git commit --amend` migrated the note onto the new SHA via
  `post-rewrite`

If any one of those failed, capture the staging dir + notes ref
contents before debugging — they're the most useful artifacts for
diagnosing hook-pipeline regressions.

## 3. Capture: simulated hook events (reproducible)

The `post-commit` hook flushes whatever is staged in `.git/prov-staging/`
into a note attached to HEAD. Section 2 confirmed Claude Code drives
this end-to-end; this section drives the same handlers with synthetic
JSON so you can hit edge cases on demand:

```bash
SID="dogfood-$(date +%s)"

# Simulate a SessionStart.
echo "{\"session_id\":\"$SID\",\"model\":\"claude-sonnet-4-5\"}" \
  | prov hook session-start

# Simulate a UserPromptSubmit (public).
echo "{\"session_id\":\"$SID\",\"prompt\":\"add a hello function\",\"cwd\":\"$SANDBOX\",\"transcript_path\":\"\"}" \
  | prov hook user-prompt-submit

# Simulate the agent's edit landing on disk.
mkdir -p src && cat > src/hello.ts <<'EOF'
export function hello(name: string) {
  return `hello, ${name}`;
}
EOF

# Simulate the PostToolUse for that edit (ranges are 1-indexed inclusive).
echo "{\"session_id\":\"$SID\",\"tool_input\":{\"file_path\":\"src/hello.ts\",\"writes\":[{\"line_start\":1,\"line_end\":3}]}}" \
  | prov hook post-tool-use

# Simulate the turn ending.
echo "{\"session_id\":\"$SID\"}" | prov hook stop

# Commit — post-commit will fire and write a note on HEAD.
git add src/hello.ts
git commit -q -m "feat: hello"
git notes --ref=refs/notes/prompts list | head    # should list HEAD's note
```

### Edge cases

```bash
# Private routing — # prov:private on the first line lands the prompt and
# its edits in .git/prov-staging/<sid>/private/, not the public dir.
SID="priv-$(date +%s)"
echo "{\"session_id\":\"$SID\",\"model\":\"claude-sonnet-4-5\"}" | prov hook session-start
echo "{\"session_id\":\"$SID\",\"prompt\":\"# prov:private\nrotate the staging credentials\",\"cwd\":\"$SANDBOX\",\"transcript_path\":\"\"}" \
  | prov hook user-prompt-submit
ls .git/prov-staging/$SID/private/    # turn-1.json present here, NOT in the public dir
ls .git/prov-staging/$SID/ | grep -v private || true

# Redaction — secrets in the prompt become [REDACTED:...] markers before staging.
SID="redact-$(date +%s)"
echo "{\"session_id\":\"$SID\",\"model\":\"x\"}" | prov hook session-start
echo "{\"session_id\":\"$SID\",\"prompt\":\"key=AKIAIOSFODNN7EXAMPLE and pat=ghp_1234567890abcdefghijABCDEFGHIJABCD\",\"cwd\":\"$SANDBOX\",\"transcript_path\":\"\"}" \
  | prov hook user-prompt-submit
grep REDACTED .git/prov-staging/$SID/turn-*.json     # AWS key and GitHub PAT both replaced

# .provignore — repo-local regex rules add custom redactors.
echo 'AcmeCorp-[A-Z0-9]{8}' > .provignore
SID="provign-$(date +%s)"
echo "{\"session_id\":\"$SID\",\"model\":\"x\"}" | prov hook session-start
echo "{\"session_id\":\"$SID\",\"prompt\":\"customer=AcmeCorp-AB12CD34\",\"cwd\":\"$SANDBOX\",\"transcript_path\":\"\"}" \
  | prov hook user-prompt-submit
grep 'REDACTED:provignore-rule' .git/prov-staging/$SID/turn-*.json
rm .provignore
```

## 4. Read surface

```bash
# Whole-file lookup: every recorded edit, ordered by recency.
prov log src/hello.ts
prov log src/hello.ts --json | jq .

# Point lookup: the prompt that produced one specific line.
prov log src/hello.ts:2

# History walk: show superseded prompts via derived_from.
prov log src/hello.ts:2 --history

# --only-if-substantial skips files <10 lines or with no notes (used by the Skill).
prov log src/hello.ts --only-if-substantial          # empty (3 lines)
seq 20 | tee src/big.ts >/dev/null && git add src/big.ts && git commit -qm "big"
prov log src/big.ts --only-if-substantial            # empty (no notes)

# Search — FTS5 over prompts.
prov search "hello"
prov search "hello" --json | jq '.results | length'
prov search --limit 1 "hello"

# Operator escaping: pure literal phrase, no syntax errors from FTS5.
prov search '"quoted-phrase"'
prov search '-foo'
```

### PR timeline

```bash
git checkout -q -b feature
# Make a captured edit on the branch (reuse the SID-driven recipe above).
prov pr-timeline --base main --head HEAD --markdown
prov pr-timeline --base main --head HEAD --json | jq .
git checkout -q main
```

Edge case worth eyeballing: a PR with >5,000 added lines should mark the
overflow under "no provenance" rather than truncating silently.

### GitHub Action — U13

The `action/` package wraps `prov pr-timeline --markdown` for CI. Smoke
the TypeScript surface from the repo root:

```bash
cd /Users/matt/Documents/GitHub/prov/action
npm ci
npm run lint                      # tsc --noEmit
npm test                          # jest — 28 cases across upsert, download, timeline
npm run build                     # ncc bundles dist/index.js (committed)
git status --porcelain dist/      # empty — committed dist/ matches the source
```

End-to-end with the rendered Markdown body (no GitHub round-trip):

```bash
cd "$SANDBOX"
git checkout -q -b feature
# Make a captured edit on the branch.
prov pr-timeline --base main --head HEAD --markdown > /tmp/timeline.md
head -3 /tmp/timeline.md          # `<!-- prov:pr-timeline -->` on line 1
grep -c '^### Session' /tmp/timeline.md   # one heading per session
git checkout -q main
```

The Action's load-bearing properties (sticky-comment upsert, marker +
bot-author filter against spoofing, 65,536-char truncation) live in
`action/src/github.ts` and are covered by `__tests__/github.test.ts`.
Author-spoof prevention is the most important guarantee: a contributor
who pre-places a comment with the marker but is not the bot must not
have it edited. The "refuses to edit a marker-spoofing comment" test
encodes that.

For a true end-to-end run you need a release with a signed binary —
v1 ships the Action ahead of the first release. Once `v0.1.x` is cut
with cosign-signed assets, point a scratch workflow at this Action
pinned to a commit SHA and verify the comment appears on a test PR.

## 5. Cache / reindex

```bash
prov reindex
prov reindex --json | jq .schema_version

# Drift recovery: write a note via raw git, confirm the cache notices.
git notes --ref=refs/notes/prompts add -f -m '{"schema_version":1,"edits":[]}' HEAD
prov log src/hello.ts                                # forces freshness check; rebuilds if stamp drifts
```

## 6. Privacy

### `prov mark-private`

```bash
SHA=$(git rev-parse HEAD)
prov mark-private "$SHA"
git notes --ref=refs/notes/prompts list | grep "$SHA" || echo "removed from public"
git notes --ref=refs/notes/prompts-private list | grep "$SHA" && echo "now private"

# Idempotent on already-private (no-op message, exit 0).
prov mark-private "$SHA"

# No-note commit (clear message, exit 0).
ROOT=$(git rev-list --max-parents=0 HEAD)
prov mark-private "$ROOT"

# Bad ref — git's error surfaces.
prov mark-private deadbeef || echo "expected failure"
```

### `prov redact-history`

```bash
# Set up a note containing a secret-looking string, then scrub it.
SID="leak-$(date +%s)"
echo "{\"session_id\":\"$SID\",\"model\":\"x\"}" | prov hook session-start
echo "{\"session_id\":\"$SID\",\"prompt\":\"the token is sk_live_supersecret\",\"cwd\":\"$SANDBOX\",\"transcript_path\":\"\"}" \
  | prov hook user-prompt-submit
echo "{\"session_id\":\"$SID\"}" | prov hook stop
echo leak >> src/hello.ts && git add . && git commit -qm "leak"
prov redact-history 'sk_live_[A-Za-z0-9]+'
git notes --ref=refs/notes/prompts show HEAD | grep -c REDACTED   # >=1

# Invalid regex must error before any rewrite.
prov redact-history 'invalid[regex' || echo "expected fail"

# Pattern that matches nothing — reports 0, exits 0.
prov redact-history 'will-not-appear-anywhere'
```

## 7. Sync (fetch / push / pre-push gate / notes-resolve)

### Push and the pre-push gate

```bash
git push -q origin main
prov push origin
# Bare remote now carries refs/notes/prompts.
git --git-dir="$PEER" for-each-ref refs/notes/

# Pre-push gate: a secret hidden in a note should block the push.
SID="badpush-$(date +%s)"
echo "{\"session_id\":\"$SID\",\"model\":\"x\"}" | prov hook session-start
# Bypass the write-time redactor by editing the note in place after capture.
echo "{\"session_id\":\"$SID\",\"prompt\":\"safe\",\"cwd\":\"$SANDBOX\",\"transcript_path\":\"\"}" \
  | prov hook user-prompt-submit
echo "{\"session_id\":\"$SID\"}" | prov hook stop
echo gate >> src/hello.ts && git add . && git commit -qm "gate"
git notes --ref=refs/notes/prompts append -m 'leaked: AKIAIOSFODNN7EXAMPLE' HEAD
prov push origin || echo "expected: gate blocked the push"

# Documented escape hatch — bypass + audit trail in staging.
prov push origin --no-verify
ls .git/prov-staging/log 2>/dev/null && echo "override logged"

# Clean up the seeded leak.
prov redact-history 'AKIA[A-Z0-9]+'
prov push origin
```

### Fetch and conflict resolution

Simulate a divergent peer to drive `prov notes-resolve` end-to-end:

```bash
CLONE="$(mktemp -d /tmp/prov-clone-XXXX)"
git clone -q "$PEER" "$CLONE"
(
  cd "$CLONE"
  prov install --enable-push origin
  prov fetch origin
  # Cause the clone to diverge: add an edit-bearing note on the same commit.
  HEAD_SHA=$(git rev-parse HEAD)
  git notes --ref=refs/notes/prompts add -f \
    -m '{"schema_version":1,"edits":[{"file":"src/hello.ts","line_range":[1,3],"prompt":"clone side"}]}' \
    "$HEAD_SHA"
  prov push origin
)

# Back in the sandbox: write a different note on the same commit, fetch, resolve.
HEAD_SHA=$(git rev-parse HEAD)
git notes --ref=refs/notes/prompts add -f \
  -m '{"schema_version":1,"edits":[{"file":"src/hello.ts","line_range":[1,3],"prompt":"sandbox side"}]}' \
  "$HEAD_SHA"
prov fetch origin || echo "merge in progress (expected)"
ls .git/NOTES_MERGE_WORKTREE/                   # one file per conflicted commit
prov notes-resolve
git notes --ref=refs/notes/prompts show "$HEAD_SHA" | jq '.edits | length'   # union >= 2
prov push origin
```

Edge cases the resolver must handle (existing tests cover these — verify
they're green before relying on the binary):

- Diff3 shared-context lines (JSON braces, `version` field) appear
  **outside** the conflict markers and must be appended to **both** sides
  to reconstruct valid JSON. See
  `docs/solutions/conventions/git-notes-merge-conflict-parsing-conventions-2026-05-03.md`.
- Entries with `tool_use_id: None` must dedupe by `(file, line_range)`,
  not collapse onto each other. Regression test:
  `squash_with_none_tool_use_id_keeps_distinct_file_regions`.
- Schema version mismatch on one side aborts that file only; re-running
  recovers.

## 8. Rewrite preservation (rebase / squash / repair)

Section 2.4 already covered the amend path with a real session. The
recipes below stress the rebase, squash, and repair codepaths the
real-session walkthrough doesn't.

### Rebase + squash (N:1 edits[] union)

```bash
git checkout -q -b r1
for i in 1 2 3; do echo "line $i" >> src/big.ts; git commit -qam "step $i"; done
GIT_SEQUENCE_EDITOR='sed -i.bak -e "2,\$ s/^pick/squash/"' git rebase -i HEAD~3
NEW=$(git rev-parse HEAD)
git notes --ref=refs/notes/prompts show "$NEW" | jq '.edits | length'   # >= sum of pre-squash edits
git checkout -q main
git branch -D r1
```

### Repair (when the hook was bypassed)

```bash
# Force orphaning by rewriting via a path that bypasses prov: capture, then
# blow away the note before the post-rewrite hook fires.
echo orphan >> src/hello.ts && git commit -qam "orphan"
ORPHAN_OLD=$(git rev-parse HEAD)
git -c core.hooksPath=/dev/null commit -q --amend --no-edit   # post-rewrite skipped
ORPHAN_NEW=$(git rev-parse HEAD)
git notes --ref=refs/notes/prompts list | grep "$ORPHAN_NEW" || echo "orphaned (expected)"

prov repair --dry-run
prov repair
git notes --ref=refs/notes/prompts list | grep "$ORPHAN_NEW" && echo "repaired"

# Re-running is idempotent (new SHA already has a note → skipped).
prov repair --json | jq '.results[] | select(.status == "skipped-existing")' | head -1
```

The repair walker matches **only terminal** rewrite events (`commit
(amend)`, `rebase (finish)`) — `rebase (pick)` and friends produce
intermediate SHAs and would migrate notes onto throwaway commits if
they were honored. Regression test: `rewrite_subjects_classified`.

## 9. Housekeeping (`prov gc`)

```bash
prov gc --dry-run --json | jq .

# Force an unreachable commit and confirm gc culls its note.
git checkout -q --detach
echo dead > src/dead.ts && git add . && git commit -qm "dead"
DEAD=$(git rev-parse HEAD)
SID="dead-$(date +%s)"
echo "{\"session_id\":\"$SID\",\"model\":\"x\"}" | prov hook session-start
echo "{\"session_id\":\"$SID\",\"prompt\":\"dead\",\"cwd\":\"$SANDBOX\",\"transcript_path\":\"\"}" | prov hook user-prompt-submit
echo "{\"session_id\":\"$SID\"}" | prov hook stop
git notes --ref=refs/notes/prompts add -f -m '{"schema_version":1,"edits":[]}' "$DEAD"
git checkout -q main                              # $DEAD now unreachable
prov gc
git notes --ref=refs/notes/prompts list | grep "$DEAD" || echo "culled"

# But: detached-HEAD WIP must NOT be culled. Regression test:
# gc_preserves_notes_for_detached_head_commits.
git checkout -q --detach
echo wip > src/wip.ts && git add . && git commit -qm "wip"
WIP=$(git rev-parse HEAD)
git notes --ref=refs/notes/prompts add -f -m '{"schema_version":1,"edits":[]}' "$WIP"
prov gc
git notes --ref=refs/notes/prompts list | grep "$WIP" && echo "wip preserved"
git checkout -q main

# Staging TTL: stale dirs prune; unreadable mtime should still prune (defensive
# polarity — see docs/solutions/.../defensive-default-polarity-conventions...).
mkdir -p .git/prov-staging/old-session
touch -t 200001010000 .git/prov-staging/old-session
prov gc --staging-ttl-days 1
ls .git/prov-staging/old-session 2>/dev/null || echo "pruned"

# --compact drops preceding_turns_summary on notes >90 days old (need real-aged
# notes to verify; on a fresh sandbox this is a no-op).
prov gc --compact --dry-run
```

## 10. Stubs (should fail loudly, not no-op)

```bash
prov backfill --yes              || echo "stub fail (expected)"

# `prov regenerate` was dropped from v1 — it should be unknown, not stubbed.
prov regenerate src/hello.ts:2 2>&1 | grep -q "unrecognized subcommand" \
  && echo "OK: regenerate is gone, not stubbed" \
  || echo "FAIL: regenerate either silently succeeded or is still wired as a stub"
```

If `prov backfill` silently succeeds, that's a regression — it's
documented as not-yet-implemented and must surface that to the user.
`prov regenerate` should be unknown to clap entirely.

## 11. Defensive-default regressions to verify

These five are the explicit polarity invariants from
`docs/solutions/conventions/defensive-default-polarity-conventions-2026-05-03.md`.
Bake them into your manual pass; each has a unit test you can re-run as a
sanity check:

1. `prune_staging` falls back to UNIX_EPOCH on read error (gets pruned),
   never `now()`. Test above under section 9.
2. Cache `notes_ref_sha` stamp survives transient `git rev-parse` errors —
   "error reading ref" is not the same as "ref absent."
3. `dedupe_and_sort_edits` includes `(file, line_range)` in the key when
   `tool_use_id` is `None`. Verified by section 7.
4. `is_reachable` augments `for-each-ref` with `merge-base --is-ancestor`
   so detached HEAD WIP is preserved. Verified by section 9.
5. `is_rewrite_subject` matches only terminal reflog events. Verified by
   section 8.

Run the targeted unit tests for cheap reassurance:

```bash
cd /Users/matt/Documents/GitHub/prov
cargo test --release \
  squash_with_none_tool_use_id_keeps_distinct_file_regions \
  gc_preserves_notes_for_detached_head_commits \
  rewrite_subjects_classified \
  split_conflict_appends_shared_prefix_to_both_sides
```

## 12. Cleanup

```bash
rm -rf "$SANDBOX" "$PEER" "$CLONE"
unset SANDBOX PEER CLONE SID SHA HEAD_BEFORE HEAD_AFTER ORPHAN_OLD ORPHAN_NEW DEAD WIP NEW ROOT HEAD_SHA
```

## Appendix: troubleshooting

- **Hook didn't fire after `prov install`.** Restart Claude Code —
  `.claude/settings.json` is read at session start. For git hooks,
  confirm `core.hooksPath` is unset (or includes `.git/hooks`) and
  inspect `.git/hooks/post-commit` for the `# >>> prov` block.
- **Note not visible after commit.** Run `prov reindex`. The post-commit
  cache write is best-effort; a stale cache reads as "no provenance."
- **`prov fetch` says "merge in progress."** Resolve with
  `prov notes-resolve`; if a partial rewrite left files behind, re-run
  the resolver — it revalidates and finalizes idempotently.
- **`prov push` blocked unexpectedly.** Inspect the offending note with
  `git notes --ref=refs/notes/prompts show <sha>`. Use
  `prov redact-history '<pattern>'` to scrub, then push without
  `--no-verify`. Reach for `--no-verify` only after rotating the secret.
