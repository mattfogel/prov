# Implementation Follow-ups

Items discovered during implementation that should be addressed before the
parent unit is considered fully done, or rolled into a later unit. Distinct
from the v1 plan's "Deferred to Implementation" section: those entries were
chosen to defer at planning time. The entries here surfaced after the unit
shipped and need a home so they don't get lost.

When closing one out, delete the entry (or move it to a `## Closed` section
with a commit/PR link).

---

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

- [ ] **Populate `original_blob_sha` on matched edits.** The post-commit
  flush currently writes `original_blob_sha: ""` because U3 does not yet
  store the AI's full original output as a git blob. Without it, U14
  (`prov regenerate`) has nothing to diff against. Capture-side fix: after
  PostToolUse stages an edit, hash-object the `after` content via
  `git hash-object --stdin -w`, store the returned SHA in the
  `EditRecord`, and propagate into the `Edit` at flush time. Owner: U14
  prerequisite; can land standalone or as part of U14.

- [ ] **Partial-match cleanup of `edits.jsonl`.** The post-commit handler
  removes the entire session dir only when *every* staged edit matched
  the commit's diff. Partial-match cleanup (rewriting `edits.jsonl` to
  drop just the matched entries) is currently deferred. Risk: a session
  with one matched edit and one unmatched edit will re-attempt the
  matched edit on the next commit and re-write it as a duplicate.
  Owner: U9 (history-rewrite handling) — it touches the same staging-tree
  invariants and benefits from being designed alongside the rewrite path.

---

## How to add an entry

```
- [ ] **Short title.** One paragraph: what's missing, where the marker is
  (file:line if in code), how to verify the fix, suggested owner.
```
