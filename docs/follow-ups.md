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

- [ ] **Live-session verification of `tool_input` / `tool_use_id` shapes.**
  The PostToolUse parser is pinned to the documented Edit/Write/MultiEdit
  shapes, with no live-payload calibration. Marker: `TODO(U3-empirical)` in
  `crates/prov-cli/src/commands/hook.rs:208`. Capture a real Claude Code
  session, diff against fixtures under
  `crates/prov-core/tests/fixtures/hook-payloads/`, and either confirm the
  shapes match or update the parser + fixtures. Owner: U3 closeout.

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
