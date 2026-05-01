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

- [ ] **Refresh the SQLite cache on post-commit note write.** When the
  post-commit handler writes a new note via `NotesStore::write`, it does
  not update `<git-dir>/prov.db`. Cache-keyed reads (`prov log <file>`
  whole-file form, `prov search`) miss until the user manually runs
  `prov reindex`. Symptom: the resolver prints `cache may be stale
  (recorded=None, live=Some(...))`. Fix: after a successful note write in
  `crates/prov-cli/src/commands/hook.rs::handle_post_commit`, upsert the
  new `Note` into the cache directly (single-note insert) and stamp
  `cache_meta.notes_ref_sha` to the post-write `refs/notes/prompts` SHA.
  A full reindex on every commit is too heavy at scale — prefer the
  targeted insert path. Surfaced while dogfooding U5; defeats the
  warm-cache promise of R3 if left as-is. Owner: U3 closeout.

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
