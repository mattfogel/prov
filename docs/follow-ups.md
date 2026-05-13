# Implementation Follow-ups

Items discovered during implementation that should be addressed before the
parent unit is considered fully done, or rolled into a later unit. Distinct
from the v1 plan's "Deferred to Implementation" section: those entries were
chosen to defer at planning time. The entries here surfaced after the unit
shipped and need a home so they don't get lost.

When closing one out, delete the entry (or move it to a `## Closed` section
with a commit/PR link).

---

## Agent harness adapters

- [ ] **Evaluate Cursor against the adapter readiness bar.** Cursor is a
  future adapter candidate, not a shipped harness. Before planning support,
  verify prompt/session/edit lifecycle hooks, whether hooks run locally and in
  cloud/remote contexts, payload stability, non-blocking behavior, privacy
  routing compatibility, and whether repo-local install/uninstall can preserve
  user config.

- [ ] **Evaluate Pi against the adapter readiness bar.** Pi appears more likely
  to require an extension-style integration than a declarative hook template.
  Validate extension packaging, lifecycle fidelity, file-edit payload shape,
  repo trust/config behavior, and whether Prov can capture without transcript
  scraping or blocking the agent loop.

## U3 — Capture pipeline (PR #3)

- [ ] **Surface "captured but not yet flushed" state to the user.** Capture
  fires on PostToolUse and lives in `<git-dir>/prov-staging/<session_id>/`
  until `git commit` runs the post-commit handler, which is when the note
  is actually written and `prov log` / `prov search` see it. The natural
  user mental model is "prov captures on PostToolUse" and the gap between
  that and "prov shows on commit" is invisible — staged sessions can sit
  for hours with no signal that they exist. Surfaced while dogfooding U5:
  user ran two Claude Code sessions, didn't commit, and was surprised
  `prov log` didn't reflect them. Options to weigh when picking this up
  (don't pre-commit a solution): (a) a `prov status` command that lists
  staged sessions and their staleness; (b) auto-include the staging count
  in `prov log` / `prov search` output when non-empty ("note: 2 staged
  sessions not yet committed"); (c) a `prov flush` command that
  synthesizes a preview note against `git diff --cached` without
  committing; (d) docs-only fix: README + Skill description make the
  capture/flush split explicit. Owner: open — likely sits with whoever
  picks up Phase 2 polish or Phase 3 Skill work, since the Skill (U12)
  also has to know about commit-vs-staging to behave correctly.

- [ ] **Partial-match cleanup of `edits.jsonl`.** The post-commit handler
  removes the entire session dir only when *every* staged edit matched
  the commit's diff. Partial-match cleanup (rewriting `edits.jsonl` to
  drop just the matched entries) is currently deferred. Risk: a session
  with one matched edit and one unmatched edit will re-attempt the
  matched edit on the next commit and re-write it as a duplicate.
  Owner: U9 (history-rewrite handling) — it touches the same staging-tree
  invariants and benefits from being designed alongside the rewrite path.

---

## U8 — Sync (fetch/push helpers + pre-push gate)

- [ ] **`prov.scanAllPushes` config to extend the gate to non-notes refs.** The
  v1 gate scopes to `refs/notes/prompts*` per R6's "negligible overhead on
  regular pushes" requirement. The plan calls out `prov.scanAllPushes` as the
  opt-in escape hatch for users who want the redactor to also scan diffs of
  every code commit being pushed. Implementing it requires walking
  `<remote-sha>..<local-sha>` and running the redactor over each commit's
  diff text — straightforward but adds latency proportional to the diff size,
  and needs care around binary blobs and very large pushes. Owner: open;
  natural pickup once a real user asks for it.

- [ ] **Audit log when `--no-verify` is used directly via `git push`.** When
  the user runs `prov push --no-verify`, the audit log records the bypass
  before invoking git (see `commands/push.rs`). When the user instead runs
  plain `git push --no-verify` (skipping `prov push` entirely), git suppresses
  every hook — there is no in-band place to record the bypass. Options: (a)
  a periodic background reconciliation that diffs the local notes ref against
  the remote tracking ref and warns if a delta exists that the gate would
  have caught; (b) docs-only — make it clear that `--no-verify` via `git
  push` directly is unaudited and should be avoided. Owner: open; revisit if
  team-mode adopters report drift.

- [ ] **Single-commit pinpointing in pre-push error messages.** The gate
  reports the *annotated commit* SHA carried by each note blob, which is
  what a user wants to investigate. But for a note attached via squash/merge
  the user may need extra context to reach the offending source line; pairing
  the SHA with the file path(s) named in the note's `edits[]` would help.
  Cheap to add — gate already parses the note. Owner: open; pickup with
  any future redactor-message polish pass.

---

## U15 — `prov backfill` (PR #45)

These items surfaced in the multi-agent code review of PR #45 but were
deferred from the in-PR fix pass — either because they're system-wide
shapes that bleed past U15's scope (`.provignore` loading, the Skill
docs gap), or because they're lower-impact polish that warrants its
own focused PR rather than bloating the safety-fix commit.

- [ ] **No locking on concurrent `prov backfill` runs.** Two simultaneous
  invocations both call `git notes add --force`; last writer wins, the
  earlier run's note silently disappears. Reproducible by running two
  `--yes` invocations against overlapping transcript sets in the same
  repo. Fix sketch: take an advisory file lock on
  `<git-dir>/prov-backfill.lock` for the duration of the candidate
  walk + write loop; refuse to start (or wait) when held. Owner: open;
  pickup when team-mode users actually report drift.

- [ ] **`confirm_or_bail` hangs in CI pseudo-TTY.** When `--yes` is
  omitted and stdin is a pty (some CI runners present one), the
  current code calls `is_terminal()` and gets `true`, then
  `read_line()` blocks indefinitely. Fix: add a read timeout via
  `crossterm` or a poll-based read, or hard-fail when no `--yes` and
  stdin is non-interactive after a brief grace. Verification: run
  `script -q -c 'prov backfill' /dev/null` (which fakes a pty) and
  confirm it errors fast instead of hanging. Owner: open.

- [ ] **Re-running backfill creates unreachable note objects each
  pass.** `process_transcript` re-writes the merged note even when the
  resulting JSON is byte-identical to the prior write. Each rewrite
  advances the notes ref and orphans the prior blob; `prov gc` cleans
  it up later but the churn is wasteful. Fix sketch: hash the new
  serialized note JSON and compare against the prior; skip the write
  when identical. Owner: open; visible in long-running team usage.

- [ ] **`--transcript-path` doesn't validate cwd against current
  repo.** A transcript whose `cwd` field references a different repo
  is silently processed against the current repo's commit history,
  producing cross-repo provenance pollution. Fix: at parse time,
  check `session.cwd` against `git.work_tree()` (after canonicalizing
  both); refuse with a clear error when they diverge. The
  `--cross-author` flag is the natural opt-out shape for users who
  want to backfill from a different machine's transcripts. Owner:
  open; revisit if a real user surfaces a legitimate cross-repo
  workflow.

- [ ] **Schema drift in transcript `tool_use` field names → silent
  empty sessions.** When Claude Code renames a field in
  `decompose_tool_use`, the parser returns an empty edits vec, the
  session has no edits, and the run-summary says "N sessions without
  a match" — indistinguishable from "no content overlap." Fix sketch:
  emit an explicit warning when a transcript parses cleanly but
  produces zero edits AND the parser hit at least one unrecognized
  `tool_use.name`; surface those names in the warning so a follow-up
  fix has a concrete target. Owner: open; the canary fires the first
  time Claude Code ships an incompatible schema.

- [ ] **`.provignore` is never loaded anywhere.** System-wide finding
  surfaced by U15's adversarial reviewer: live capture
  (`hook.rs::user-prompt-submit`), the pre-push gate, AND backfill
  all use `Redactor::new()` with built-in detectors only — none load
  `.provignore`. The plan's R4 calls out per-project regex rules as a
  v1 capability but the wiring is missing. Backfill amplifies this
  into a mass-secret-leak shape if a team has been depending on
  `.provignore` for project-specific patterns. Fix: add
  `Redactor::with_provignore(repo_root)` and call it from all three
  surfaces; add a regression test that verifies a `.provignore`
  pattern redacts in each. Owner: blocking before any push-by-default
  posture is restored.

- [ ] **Skill silent on `prov backfill` and approximate-note
  weighting.** `plugin/skills/prov/SKILL.md` teaches `prov log` only.
  An agent asked "bootstrap provenance for this repo" has no
  documented path to `prov backfill --yes`, and the `(approximate)`
  marker that backfill stamps onto reconstructed notes is in the JSON
  envelope but the Skill doesn't tell the agent to weight approximate
  notes lower than live captures. Fix: add a bootstrapping section
  documenting `--yes` (required in agent context, since stdin is
  never a TTY) and an "interpreting backfilled notes" section in
  `references/querying.md` with a JSON example showing
  `"approximate": true` and `"approximate_confidence": 0.83`. Owner:
  pickup with whoever next touches the Skill.

- [ ] **`MAX_COMMITS=5_000` cap silently drops older commits.**
  `load_candidate_commits` walks at most 5000 commits with no warning
  when the repo is larger. Older matching commits surface as
  `skipped_no_match` — indistinguishable from a session with no real
  candidate. Fix: when `git rev-list --count HEAD > MAX_COMMITS`,
  warn at run start naming the cap and exposing a `--max-commits`
  override. Owner: open.

- [ ] **`parse_unified_diff_added` confuses content lines starting
  with `++ b/` as file headers.** Unified diff prepends `+` to every
  added line; a source line whose text is verbatim `++ b/foo` becomes
  `+++ b/foo` in the diff and is consumed by `strip_prefix("+++ b/")`.
  The parser switches `current_file` to a bogus path, drops added
  lines, and corrupts the per-file map until the next real `+++ b/`.
  Real-world hits are uncommon (markdown / commented diffs) but
  possible. Fix: gate `+++ b/` interpretation on having just seen
  `--- a/` on the prior line, or use `git diff --raw -z` boundaries.
  Owner: open.

- [ ] **`locate_file_in_diff` non-deterministic on ambiguous suffix
  matches.** When two diff keys are both suffixes of the captured
  absolute path (e.g., `src/main.rs` and `lib/main.rs` both in one
  commit, captured path ends with `main.rs`), the function returns
  whichever HashMap iteration visits first — meaning re-running
  backfill against the same transcripts can produce different
  attribution and break idempotency. Fix: collect every suffix-
  matching key, return the one with the longest common-suffix length
  (most specific). Verification: add a test with two candidate keys
  that are both suffixes of the captured path and assert the longer
  key wins. Owner: open.

---

## docs/solutions — refresh hints

- [ ] **Refresh `git-subprocess-hardening-conventions-2026-05-02.md` Pattern 7
  when issue #29 lands.** Pattern 7 currently gives a manual `git reflog
  expire --expire=now --all && git gc --prune=now` recipe as the remediation
  for `prov redact-history` not scrubbing residuals. Once #29 ships a
  built-in `prov gc-secrets` (or `prov redact-history --gc`), Pattern 7
  becomes stale: it will describe a manual workflow the CLI now automates
  and won't cross-reference the polarity convention in
  `defensive-default-polarity-conventions-2026-05-03.md` that explains *why*
  the prune cutoff has to be defensive. Verification: run
  `/ce-compound-refresh git-subprocess-hardening-conventions-2026-05-02`
  after #29 closes; confirm Pattern 7 names the new subcommand and links to
  the polarity doc. Owner: whoever closes #29.

---

## How to add an entry

```
- [ ] **Short title.** One paragraph: what's missing, where the marker is
  (file:line if in code), how to verify the fix, suggested owner.
```
