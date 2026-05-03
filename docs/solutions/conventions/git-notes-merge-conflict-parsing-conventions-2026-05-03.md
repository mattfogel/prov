---
title: Why the git notes merge conflict parser appends Outside-block lines to both sides
date: 2026-05-03
category: conventions
module: prov-cli
problem_type: convention
component: tooling
severity: high
applies_when:
  - parsing files in .git/NOTES_MERGE_WORKTREE/ produced by git notes merge with notes.mergeStrategy=manual
  - implementing or reviewing any split_conflict-style parser for git notes merge output
  - encountering non-empty content before the first conflict marker in a notes conflict file
  - implementing a custom git merge driver for JSON, YAML, TOML, or any multi-line structured content stored in notes
tags:
  - git-notes
  - conflict-parsing
  - diff3
  - notes-resolve
  - false-positive-prevention
  - cli-hardening
  - rust
related_components:
  - development_workflow
---

# Why the git notes merge conflict parser appends Outside-block lines to both sides

## Context

Prov stores prompt-capture data as pretty-printed JSON blobs attached to commits via `refs/notes/prompts`. When two developers annotate the same commit on different machines and sync, the notes ref diverges. `prov install` sets `notes.mergeStrategy=manual`, so when `prov fetch` pulls the remote ref and the histories diverged, git parks each conflicting commit's note as a conflict file in `.git/NOTES_MERGE_WORKTREE/<commit-sha>` and returns non-zero rather than auto-resolving. `prov notes-resolve` then reads those files, parses both sides as `Note` structs, unions their `edits[]` arrays, and finalizes via `git notes merge --commit`.

The note blobs are pretty-printed JSON — not compact — because that makes them human-readable and gives `git diff` meaningful output when a single note changes across commits. That formatting choice has a non-obvious interaction with how git's diff3 merge engine writes conflict files.

When `git notes merge` runs with `notes.mergeStrategy=manual` on two multi-line note blobs, git's textual diff3 engine identifies lines that are identical on both sides and treats them as **shared context**. Those shared-context lines are placed **outside** the `<<<<<<<` / `=======` / `>>>>>>>` markers. For pretty-printed JSON notes, the shared-context lines are typically the structural envelope: the opening `{`, the `"version": 1,` field, and the closing `}`. Only the differing interior (`"edits": [...]`) lands inside the markers.

This behavior caused a false-positive review incident on PR #33 (session history). A multi-agent code review — three reviewers, cross-promoted to anchor confidence 100 — flagged `split_conflict`'s "append Outside to both" behavior as silent corruption. The proposed fix was to error on any non-whitespace content outside the conflict markers, on the grounds that such content must be a malformed file. The reasoning looked correct from the code alone. When a fixer attempted to apply the change, 2 of the 4 integration tests failed immediately: the very first line of every real conflict file produced by `git notes merge` on pretty-printed JSON is `{`, which is non-whitespace and is always outside the markers. The reviewers had reasoned about a hypothetical (Outside = garbage) without running the actual git binary against multi-line content. The conventional wisdom "text outside the conflict markers is suspicious" is wrong for diff3 manual merge.

This document exists so the next reviewer who reads `split_conflict` and reaches for the same "fix" has something concrete to point at before making the change.

## Guidance

### The invariant: Outside lines belong to both sides

In a conflict file produced by `git notes merge` with pretty-printed multi-line JSON, lines **outside** the conflict markers are diff3-identified shared context that genuinely appears in both the local and the incoming version of the blob. A correct parser **must append those lines to both reconstructed buffers**. Treating them as suspicious, skipping them, or erroring on them will cause the JSON parser to receive a fragment that is missing its structural envelope — it will reject every realistic conflict file the moment notes are pretty-printed.

### The state machine in `notes_resolve.rs`

`split_conflict` (lines 178–220 of `crates/prov-cli/src/commands/notes_resolve.rs`) implements a three-state machine:

```rust
enum ConflictState {
    Outside,   // before first <<<<<<< or after >>>>>>> — shared context
    Local,     // between <<<<<<< and =======
    Incoming,  // between ======= and >>>>>>>
}
```

Each arm's behavior:

| State | Action |
|---|---|
| `Local` | append line to `local` buffer only |
| `Incoming` | append line to `incoming` buffer only |
| `Outside` | append line to **both** `local` and `incoming` buffers |

The `Outside` arm at line 201 appends each line to both buffers and carries an explicit comment explaining why. The function's own docstring states the invariant and its rationale. Do not remove or weaken either.

### The regression test that pins this

`split_conflict_appends_shared_prefix_to_both_sides` in the same file at line 416 constructs a conflict body where the opening `{`, `"version": 1,` line, and closing `}` are outside the markers, then asserts that both reconstructed sides contain the version field and end with `}`. If the double-append is removed, this test fails. The test is the primary guard against re-introducing the false fix.

### One additional empirical finding

`git notes merge` does **not** honor `merge.conflictstyle=diff3` even when that option is set globally (verified on git 2.50.1, session history). The notes merge machinery uses its own built-in conflict style. The conflict files in `NOTES_MERGE_WORKTREE` always use 2-way markers (`<<<<<<<` / `=======` / `>>>>>>>`), not the 3-way `||||||| ancestor` form. The shared-context lines outside the markers are the mechanism git uses to communicate what it would have put into the ancestor section. A parser that looks for the diff3 ancestor marker will never find it and will misinterpret the file structure.

## Why This Matters

A parser that validates Outside content as suspicious silently corrupts every realistic merge of multi-line JSON notes. If `split_conflict` returned an error on non-whitespace Outside content, `prov notes-resolve` would refuse to resolve any conflict involving pretty-printed notes — which is every conflict produced by the current note serializer — and the user would be left with a permanent merge-in-progress state, forced to abort and reconcile by hand.

The mistake is structurally tempting because the Outside lines often look like leftover content at first glance. In a textual diff3 conflict between two compact (single-line) JSON blobs, there would be no Outside content — all the text would be inside the markers. The switch from compact to pretty-printed serialization is what activates the shared-context behavior, and a reviewer who has only seen compact-JSON conflict files has no mental model for the non-empty Outside case.

Without this documentation, every future reviewer encountering `split_conflict` faces the same false-positive path: read the Outside arm, notice it handles non-whitespace content without complaint, conclude it must be a bug, propose the "fix." Three reviewers reached that conclusion in a single review pass, all at high confidence. The fixer caught it only because they ran the test suite against the actual git binary.

## When to Apply

Apply this guidance — and re-read it before changing `split_conflict` — in any of these situations:

- Parsing files from `.git/NOTES_MERGE_WORKTREE/` (the current resolver: `prov notes-resolve`)
- Implementing a new parser or deserializer for `git notes merge` conflict files, regardless of language
- Adding a custom git merge driver for JSON, YAML, TOML, or any other structured format stored in notes — if the blobs are multi-line, the driver will receive files with the same Outside-content structure
- Reading conflict-marked files from any git operation that uses the diff3 textual merge engine when the conflicting blobs share structural prefix/suffix lines (multi-line JSON objects, pretty-printed YAML with shared top-level keys, TOML with shared section headers)

The rule generalizes: **if git diff3 can identify lines as identical between the two blobs, those lines will appear as Outside context in the conflict file, and a parser must route them to both sides.**

## Examples

### A representative conflict file

Two developers both annotated the same commit. Their notes share the envelope but differ only in `edits`:

```
{
  "version": 1,
<<<<<<< refs/notes/prompts
  "edits": [
    { "conversation_id": "sess_a", "turn_index": 0, ... }
  ]
=======
  "edits": [
    { "conversation_id": "sess_b", "turn_index": 5, ... }
  ]
>>>>>>> refs/notes/origin/prompts
}
```

Lines outside the markers: `{`, `  "version": 1,`, `}`. Lines inside Local: the `edits` array for `sess_a`. Lines inside Incoming: the `edits` array for `sess_b`.

### Reconstructed local side (correct)

Outside lines + Local lines:

```json
{
  "version": 1,
  "edits": [
    { "conversation_id": "sess_a", "turn_index": 0, ... }
  ]
}
```

### Reconstructed incoming side (correct)

Outside lines + Incoming lines:

```json
{
  "version": 1,
  "edits": [
    { "conversation_id": "sess_b", "turn_index": 5, ... }
  ]
}
```

### The naive "validate Outside" implementation that breaks

```rust
ConflictState::Outside => {
    // WRONG: this rejects every realistic conflict file produced
    // by git notes merge on pretty-printed JSON.
    if !line.trim().is_empty() {
        return Err(anyhow!("unexpected content outside conflict markers: {line}"));
    }
}
```

This fires on the very first `{` of every conflict file and makes `prov notes-resolve` unusable.

### The correct double-append implementation

From `crates/prov-cli/src/commands/notes_resolve.rs`, the `ConflictState::Outside` arm at line 201:

```rust
ConflictState::Outside => {
    // git's diff3-style merge places shared prefix/suffix
    // (matching JSON braces, version field, etc.) OUTSIDE the
    // markers — those lines genuinely belong to both sides.
    // Appending them to both buffers reconstructs each side's
    // original full body so the JSON parser sees a valid note.
    local.push_str(line);
    local.push('\n');
    incoming.push_str(line);
    incoming.push('\n');
}
```

The regression test at line 416 (`split_conflict_appends_shared_prefix_to_both_sides`) uses a synthetic conflict body with `{` and `"version": 1,` outside the markers and asserts that both reconstructed sides contain the version field and end with `}`. That test must stay green. If it fails after a change to `split_conflict`, the change is wrong.

**Summary for the future reviewer about to flag the double-append:** git diff3 puts structurally identical lines outside the conflict markers on purpose. Those lines belong to both sides. The double-append is correct. Run `cargo test split_conflict_appends_shared_prefix_to_both_sides` before proposing any change to the Outside arm. If that test still passes after your proposed change, re-examine whether you are testing the right thing. The empirical reference: git 2.50.1, `notes.mergeStrategy=manual`, pretty-printed JSON note blobs.

## Related

- [`defensive-default-polarity-conventions-2026-05-03.md`](./defensive-default-polarity-conventions-2026-05-03.md) — Pattern 2 documents the dedup key shape (`hook.rs::dedupe_and_sort_edits`) that the parser's output ultimately feeds. Outside-block content → JSON parse → `Note` struct → deduper. Upstream/downstream in the same data pipeline.
- [`git-subprocess-hardening-conventions-2026-05-02.md`](./git-subprocess-hardening-conventions-2026-05-02.md) — companion conventions doc for `crates/prov-cli`; Pattern 6 (remediation hints tied to commands that exist) is the closest touch point and references `prov notes-resolve` by name.
- GitHub issue [#35](https://github.com/mattfogel/prov/issues/35) — adjacent: asks whether `invalidate_cache_per_sha` covers SHAs that took the no-marker branch in `split_conflict` (auto-merged by git, not parked as conflicts). Directly motivated by the same code path documented here.
- GitHub issue [#34](https://github.com/mattfogel/prov/issues/34) — adjacent: same command, separate gap (missing `--json` flag for agent-friendly output).
