# Prov Dogfooding & End-to-End Testing Guide

A by-hand walkthrough of every shipped feature, with edge cases the test
suite has caught regressions on at least once. Use this when validating a
release candidate, after a refactor that touches a hot path, or when you
just want to feel the surface from a user's seat.

The capture flow should be exercised first through **real agent sessions**.
Section 2 drives Claude Code and Codex end-to-end because that is the only way
to prove each harness emits payloads that `prov hook ...` actually consumes.
Synthetic payload coverage still matters, but it belongs in the automated
fixture tests listed later in this guide, not in the primary manual path.

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
pwd                 # remember this path; cleanup at the end
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
test ! -e .claude/settings.json   # no Claude adapter config yet
test ! -e .codex/hooks.json       # no Codex adapter config yet
git config --get notes.displayRef         # refs/notes/prompts
git config --get notes.mergeStrategy      # manual
git config --get notes.rewrite.amend      # false
test -f .git/prov.db

prov install --agent all
jq '.hooks | keys' .claude/settings.json
jq '.hooks | keys' .codex/hooks.json
grep -n 'hooks = true' .codex/config.toml
```

### Edge cases

```bash
# Idempotent re-install: same on-disk state, no duplicate hook blocks.
prov install --agent all
grep -c '# >>> prov' .git/hooks/post-commit          # exactly 1
grep -c '"command": "prov hook' .claude/settings.json # exactly 4
grep -c '"command": "prov hook codex' .codex/hooks.json # exactly 4

# Real-user hook composition: if this repo already has a post-commit hook,
# verify prov wraps its own block without deleting the existing script body.
# Do this in a repo that already has a hook; avoid inventing one just for the
# dogfood pass.
sed -n '1,120p' .git/hooks/post-commit
prov install --agent all
sed -n '1,120p' .git/hooks/post-commit

# Team-mode opt-in (writes a fetch refspec, arms the pre-push gate).
prov install --agent all --enable-push origin
git config --get-all remote.origin.fetch             # includes refs/notes/prompts:refs/notes/origin/prompts

# Uninstall removes prov-managed lines but leaves your data.
prov uninstall
grep -c '# >>> prov' .git/hooks/post-commit          # 0
test -f .git/prov.db                                 # cache preserved
git for-each-ref refs/notes/                          # notes ref untouched

# --purge also drops cache and staging.
prov install --agent all
prov uninstall --purge
test ! -f .git/prov.db
test ! -d .git/prov-staging

# Uninstall is idempotent.
prov uninstall --purge
prov uninstall --purge   # no error
```

Reinstall before continuing:

```bash
prov install --agent all --enable-push origin
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
test ! -d "$SANDBOX/.claude"

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
ls .git/prov-staging/
cd "$SANDBOX"
rm -rf "$PLUGIN_SANDBOX"
```

The automated layout lints in `crates/prov-cli/tests/cli_plugin_layout.rs`
catch frontmatter regressions; the manual run above confirms the
marketplace install path actually carries a session through capture.

## 2. End-to-end with real Claude Code and Codex sessions

Run both harness paths when both tools are available. These are the primary
dogfooding tests because they exercise real harness payloads, real file edits,
real commits, and the shared read surface. For Codex, the repo-local `.codex/`
config layer must be trusted and the installed hooks must be reviewed in
`/hooks` before project hooks run.

### 2.1 Claude Code capture, read, search

Restart Claude Code after `prov install --agent claude`; `.claude/settings.json`
is read at session start.

```bash
cd "$SANDBOX"
claude       # or however you launch Claude Code
```

Use a normal multi-turn task that causes real `Edit`, `Write`, or `MultiEdit`
tool calls:

> Turn 1: Create `src/greet.ts` exporting a `greet(name: string)` function that
> returns a friendly greeting. Add a brief test in `src/greet.test.ts`.
>
> Turn 2: Tighten the error path so an empty name throws a `TypeError` with a
> clear message.
>
> Turn 3: Extract the greeting template into a `templates.ts` module so it can
> be reused.

Exit Claude Code, then verify the normal user path:

```bash
ls .git/prov-staging/
ls .git/prov-staging/*/turn-*.json
ls .git/prov-staging/*/edits.jsonl

git add . && git commit -qm "feat: greet with claude"
git notes --ref=refs/notes/prompts show HEAD | jq '.edits | length'
git notes --ref=refs/notes/prompts show HEAD | jq '.edits[0].tool'
prov log src/greet.ts
prov log src/greet.ts:1
prov search "greet"
```

Expected: the note contains at least one edit with `"tool": "claude-code"`, and
the CLI read surfaces return the real prompt text from the Claude Code session.

### 2.2 Codex capture, read, search

Reopen Codex in the sandbox after `prov install --agent codex`; Codex must trust
the repo-local `.codex/` config and you must approve the installed hooks in
`/hooks` before project hooks run.

Use a normal task that causes Codex to edit files with its file-edit tool:

> Create `src/codex_greet.ts` exporting `codexGreet(name: string)`. Add a small
> validation branch for empty names and a simple test file. Keep the
> implementation compact.

After Codex finishes, verify through the same user path:

```bash
ls .git/prov-staging/
ls .git/prov-staging/*/turn-*.json
ls .git/prov-staging/*/edits.jsonl

git add . && git commit -qm "feat: greet with codex"
git notes --ref=refs/notes/prompts show HEAD | jq '.edits | length'
git notes --ref=refs/notes/prompts show HEAD | jq '.edits[0].tool'
prov log src/codex_greet.ts
prov log src/codex_greet.ts:1
prov search "codexGreet"
```

Expected: the note contains at least one edit with `"tool": "codex"`, and no
Codex-specific read command is needed.

### 2.3 Private routing through a real harness

Run this once through Claude Code and once through Codex if possible. Start the
agent prompt with the opt-out marker:

> ```
> # prov:private
>
> Add a placeholder `loadStagingCredentials()` to `src/secrets.ts`.
> ```

Then verify that the commit writes to the private ref only:

```bash
SID=$(ls -t .git/prov-staging | head -1)
ls .git/prov-staging/$SID/private/
test ! -e .git/prov-staging/$SID/turn-0.json

git add . && git commit -qm "feat: secrets stub"
test -z "$(git notes --ref=refs/notes/prompts show HEAD 2>/dev/null)"
git notes --ref=refs/notes/prompts-private show HEAD | jq '.edits | length'

prov log src/secrets.ts
prov push origin
test -z "$(git --git-dir="$PEER" for-each-ref refs/notes/prompts-private)"
```

### 2.4 Redaction through a real harness

Run this through a real agent session, not by piping a synthetic prompt into
`prov hook`. Paste known-format test credentials into the prompt itself:

> Use the AWS access key `AKIAIOSFODNN7EXAMPLE` and the GitHub token
> `ghp_1234567890abcdefghijABCDEFGHIJABCD` to wire up a placeholder credential
> provider in `src/creds.ts`.

The values above are public test fixtures, not live credentials. After the
agent edits and you commit:

```bash
SID=$(ls -t .git/prov-staging | head -1)
grep REDACTED .git/prov-staging/$SID/turn-*.json
git add . && git commit -qm "feat: creds stub"
git notes --ref=refs/notes/prompts show HEAD | grep -c REDACTED
test -z "$(git notes --ref=refs/notes/prompts show HEAD | grep -E 'AKIA|ghp_')"
```

### 2.5 Rewrite migration (real session + amend)

After capturing real turns above, exercise the post-rewrite hook with
an amend:

```bash
HEAD_BEFORE=$(git rev-parse HEAD)
# Make a small real edit to a captured file with your editor, then amend.
$EDITOR src/greet.ts
git commit -qa --amend --no-edit
HEAD_AFTER=$(git rev-parse HEAD)
git notes --ref=refs/notes/prompts list | grep "$HEAD_AFTER"
test -z "$(git notes --ref=refs/notes/prompts list | grep "$HEAD_BEFORE")"
```

### 2.6 Skill — agent-side trigger fidelity (U12)

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

### 2.7 What "working end-to-end" means here

By the end of section 2 you should have observed all of:

- Real Claude Code prompts staged into `.git/prov-staging/<session-id>/`
- Real Codex prompts and `apply_patch` edits staged into the same tree
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

## 3. Fixture-backed hook coverage

Do not use synthetic `echo ... | prov hook ...` recipes as the primary
dogfooding path. They prove the parser accepts one invented shape; they do not
prove Claude Code or Codex still emits that shape. Use the real harness flows in
section 2 first, then run the automated fixture coverage when you need
reproducible edge-case confidence:

```bash
cargo test -p prov --test hook_capture
cargo test -p prov --test cli_codex_layout
cargo test -p prov-core schema::tests::non_claude_tool_value_roundtrips
```

Those tests cover:

- Legacy Claude hook commands and adapter-qualified Claude commands.
- Codex `SessionStart`, `UserPromptSubmit`, `PostToolUse` with `apply_patch`,
  and `Stop` fixtures.
- Private routing, redaction, malformed JSON, missing transcript fallback, and
  end-to-end post-commit flushing into normal notes.

When a real dogfood session fails but these tests pass, trust the real session:
the harness payload contract probably changed, or the repo-local harness config
did not load.

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
test -z "$(git notes --ref=refs/notes/prompts list | grep "$SHA")"
git notes --ref=refs/notes/prompts-private list | grep "$SHA"

# Idempotent on already-private (no-op message, exit 0).
prov mark-private "$SHA"

# No-note commit (clear message, exit 0).
ROOT=$(git rev-list --max-parents=0 HEAD)
prov mark-private "$ROOT"

# Bad ref — git's error surfaces.
! prov mark-private deadbeef
```

### `prov redact-history`

```bash
# The real-session redaction check in section 2.4 proves write-time redaction.
# For retroactive history scrubbing, use a branch/repo where you already have a
# known bad note from older capture, then scrub it:
prov redact-history '<known leaked pattern>'
git notes --ref=refs/notes/prompts show HEAD | grep -c REDACTED

# Invalid regex must error before any rewrite.
! prov redact-history 'invalid[regex'

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
# Start from a real captured note, then deliberately seed the note with a
# known-format test secret to verify the push gate catches old/bad data.
$EDITOR src/greet.ts
git add . && git commit -qm "gate"
git notes --ref=refs/notes/prompts append -m 'leaked: AKIAIOSFODNN7EXAMPLE' HEAD
! prov push origin

# Documented escape hatch — bypass + audit trail in staging.
prov push origin --no-verify
grep no-verify .git/prov-staging/log

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
! prov fetch origin
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

Section 2.5 already covered the amend path with a real session. The
recipes below stress the rebase, squash, and repair codepaths the
real-session walkthrough doesn't.

### Rebase + squash (N:1 edits[] union)

```bash
git checkout -q -b r1
# Make three small real edits to a captured file, committing after each edit.
$EDITOR src/big.ts && git commit -am "step 1"
$EDITOR src/big.ts && git commit -am "step 2"
$EDITOR src/big.ts && git commit -am "step 3"
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
$EDITOR src/hello.ts && git commit -am "orphan"
ORPHAN_OLD=$(git rev-parse HEAD)
git -c core.hooksPath=/dev/null commit -q --amend --no-edit   # post-rewrite skipped
ORPHAN_NEW=$(git rev-parse HEAD)
test -z "$(git notes --ref=refs/notes/prompts list | grep "$ORPHAN_NEW")"

prov repair --dry-run
prov repair
git notes --ref=refs/notes/prompts list | grep "$ORPHAN_NEW"

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
$EDITOR src/dead.ts
git add . && git commit -qm "dead"
DEAD=$(git rev-parse HEAD)
git notes --ref=refs/notes/prompts add -f -m '{"schema_version":1,"edits":[]}' "$DEAD"
git checkout -q main                              # $DEAD now unreachable
prov gc
test -z "$(git notes --ref=refs/notes/prompts list | grep "$DEAD")"

# But: detached-HEAD WIP must NOT be culled. Regression test:
# gc_preserves_notes_for_detached_head_commits.
git checkout -q --detach
$EDITOR src/wip.ts
git add . && git commit -qm "wip"
WIP=$(git rev-parse HEAD)
git notes --ref=refs/notes/prompts add -f -m '{"schema_version":1,"edits":[]}' "$WIP"
prov gc
git notes --ref=refs/notes/prompts list | grep "$WIP"
git checkout -q main

# Staging TTL: stale dirs prune; unreadable mtime should still prune (defensive
# polarity — see docs/solutions/.../defensive-default-polarity-conventions...).
mkdir -p .git/prov-staging/old-session
touch -t 200001010000 .git/prov-staging/old-session
prov gc --staging-ttl-days 1
test ! -e .git/prov-staging/old-session

# --compact drops preceding_turns_summary on notes >90 days old (need real-aged
# notes to verify; on a fresh sandbox this is a no-op).
prov gc --compact --dry-run
```

## 10. Stubs (should fail loudly, not no-op)

```bash
! prov backfill --yes

# `prov regenerate` was dropped from v1 — it should be unknown, not stubbed.
! prov regenerate src/hello.ts:2
prov regenerate src/hello.ts:2 2>&1 | grep -q "unrecognized subcommand"
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

- **Hook didn't fire after `prov install`.** Restart the agent harness.
  Claude Code reads `.claude/settings.json` at session start; Codex must trust
  the repo-local `.codex/` layer and review hooks in `/hooks` before project
  hooks run. For git hooks,
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
