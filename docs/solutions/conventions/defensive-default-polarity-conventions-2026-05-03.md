---
title: Defensive-default polarity, dedupe-key nullability, and predicate blind-spot conventions
date: 2026-05-03
category: conventions
module: prov-cli
problem_type: convention
component: tooling
severity: high
applies_when:
  - writing helpers that prune, expire, or cull data based on a fallback value
  - building dedupe keys that contain nullable components
  - implementing reachability or set-membership predicates that delegate to a single git tool
  - matching reflog or git event names to classify a multi-step process
  - adding a new write or admin prov subcommand
tags:
  - defensive-defaults
  - polarity
  - dedupe-keys
  - reachability
  - reflog
  - agent-safety
  - cli-hardening
  - rust
related_components:
  - development_workflow
---

# Defensive-default polarity, dedupe-key nullability, and predicate blind-spot conventions

## Context

A catch-up review of PR #32 (`feat/u9-history-rewrite`, fix commit `8f27480`) — an 8-reviewer parallel pass over the new `hook.rs::handle_post_rewrite`, `repair.rs`, and `gc.rs` — surfaced five bugs that initially look unrelated but share the same underlying failure shape: a defensive measure whose internal logic was wrong polarity for the operation it guarded, plus one bug where a dedupe key with a nullable component silently collapsed distinct values. The common thread is that each produced **silent data loss or silent accumulation** with no error returned to the caller. In a provenance tool, silent loss is the worst failure class: the user gets no signal until they need the data and discover it's gone.

This doc records the patterns that caused them and the conventions that catch each one going forward. It is a companion to `git-subprocess-hardening-conventions-2026-05-02.md`, which covers the layer below: how prov shells out to git safely. The conventions here cover the layer above: how helpers, dedupe keys, reachability predicates, reflog classifiers, and CLI output contracts should be designed.

## Guidance

### 1. Defensive-default polarity must be reasoned about per-operation, not "what looks safe in isolation"

Every `unwrap_or` and every `Err`-to-`None` coercion has an implicit polarity: the fallback value either pushes toward action (prune it, migrate it, treat it as present) or toward inaction (keep it, skip it, treat it as absent). The fallback that feels safe in the abstract is often wrong for the specific operation.

**Prune cutoff — wrong polarity flipped:**
```rust
// gc.rs — before
let last_mtime = newest_mtime(&dir).unwrap_or_else(SystemTime::now);
// Reads as "be safe — assume fresh". For a prune cutoff this means
// "never falls past cutoff; survives forever". Empty/unreadable session
// dirs accumulate without bound. `prov gc --staging-ttl-days 14` does nothing.
```

```rust
// gc.rs — after
let last_mtime = newest_mtime(&dir).unwrap_or(UNIX_EPOCH);
// "Treat as maximally stale." Falls past any cutoff → gets pruned.
// An empty session dir that can't be read has no user data worth keeping.
```

**Cache stamp on Err — wrong polarity flipped:**
```rust
// hook.rs / repair.rs — before
let _ = cache.set_recorded_notes_ref_sha(public.ref_sha().ok().flatten().as_deref());
// `.ok().flatten()` silently folds both "no ref yet" and "transient
// `git rev-parse` failure" into None. Setting None wipes the recorded
// ref stamp, forcing a cold reindex on next read even though the
// migration succeeded.
```

```rust
// common.rs::invalidate_cache_per_sha — after
if let Ok(Some(sha)) = public.ref_sha() {
    let _ = cache.set_recorded_notes_ref_sha(Some(&sha));
}
// On Err, leave the prior stamp alone. Don't conflate "error reading ref"
// with "ref is absent".
```

The rule: before writing any fallback value, state aloud what action that fallback triggers. If the action is the opposite of what the error case warrants, flip the polarity or skip the write entirely.

### 2. Dedupe keys with nullable parts need a structural fallback for the None case

A dedupe key containing an `Option<_>` field collapses every `None` entry onto a single key. If multiple legitimate distinct values happen to have `None` in that position, all but one are silently dropped on the next dedupe pass.

**Nullable `tool_use_id` — wrong key shape:**
```rust
// hook.rs — before
type Key = (String, u32, Option<String>);
// (conversation_id, turn_index, tool_use_id)
// MultiEdit-decomposed edits and `prov backfill` notes both produce
// tool_use_id = None. Three distinct file regions in the same turn share
// the same key and collapse to one on squash. 2/3 of edits lost, no error.
```

```rust
// hook.rs::dedupe_and_sort_edits — after
type Key = (String, u32, Option<String>, String);
let fallback = if e.tool_use_id.is_none() {
    format!("{}@{}-{}", e.file, e.line_range[0], e.line_range[1])
} else {
    String::new() // empty discriminator when tool_use_id provides uniqueness
};
let key: Key = (
    e.conversation_id.clone(),
    e.turn_index,
    e.tool_use_id.clone(),
    fallback,
);
```

The empty-string discriminator when `tool_use_id.is_some()` is deliberate: the two shapes (`Some(_)` with empty fallback, `None` with structural fallback) occupy disjoint regions of the key space and cannot collide.

The rule: any dedupe key with an `Option<_>` field must also carry a structural discriminator that is active precisely when that field is `None`. Pick the discriminator from fields guaranteed to differ across the rows that share the `None` value.

### 3. Reachability predicates must enumerate the tool's blind spots

`git for-each-ref --contains <sha>` returns empty when the commit is reachable only through detached HEAD — git does not treat HEAD as a ref for this query. A reachability check that relies solely on `for-each-ref` will incorrectly classify detached-HEAD WIP as unreachable, and `prov gc` will silently cull its note.

```rust
// gc.rs — before
fn is_reachable(git: &Git, commit_sha: &str) -> bool {
    if git.run(["cat-file", "-e", commit_sha]).is_err() { return false; }
    match git.capture(["for-each-ref", "--contains", commit_sha]) {
        Ok(s) => !s.trim().is_empty(),
        Err(_) => false,
    }
}
// Blind to detached HEAD. for-each-ref returns empty, function returns
// false, gc culls the note for a commit the user is actively working on.
```

```rust
// gc.rs::is_reachable — after
fn is_reachable(git: &Git, commit_sha: &str) -> bool {
    if git.run(["cat-file", "-e", commit_sha]).is_err() {
        return false;
    }
    if let Ok(s) = git.capture(["for-each-ref", "--contains", commit_sha]) {
        if !s.trim().is_empty() {
            return true;
        }
    }
    git.run(["merge-base", "--is-ancestor", commit_sha, "HEAD"])
        .is_ok()
}
```

Two fallbacks were tried and rejected before settling on `merge-base --is-ancestor`:

- **`git rev-list --reflog --contains <sha>`** — `--contains` is a `for-each-ref` flag, not a `rev-list` flag. The combination does not exist; git errors out. Verify flag combinations with a real shell invocation before coding them.
- **`git reflog --all --format=%H` + grep** — over-inclusive. It included tips of deleted branches still pinned only by the reflog, breaking existing tests that expected those to be culled. A defensive fallback that's too broad creates new bugs (over-preserving) just as a default that's too narrow does (under-preserving).

`merge-base --is-ancestor <sha> HEAD` is the narrowest correct addition: it covers exactly the case where the commit is an ancestor of the current HEAD, which is what detached-HEAD work produces. The reflog-only-reachable case is intentionally kept as "unreachable" — git's own gc expires those reflog entries on its schedule, and prov's tracking should match the visible-refs view.

The rule: when implementing a reachability or set-membership predicate, document which git objects each query covers and which it does not. Add targeted fallbacks for the documented blind spots. Verify the flag combination exists before coding it.

### 4. Match the terminal event in a multi-step process, not every step

`git rebase -i` emits multiple reflog entries per run: `rebase (start)`, one `rebase (pick)` per commit, optional `rebase (squash)` and `rebase (fixup)` entries, then `rebase (finish)`. A repair function that matches every entry beginning with `"rebase"` will pair consecutive entries for each intermediate step, producing `(intermediate_sha, intermediate_sha)` pairs. If a note is attached to an intermediate SHA (which happens when `post-commit` fires during a rebase step), repair migrates it onto an unrelated commit. That commit may then be outside the notes ref's expected set, and `prov gc` culls it on the next run. Two-step silent destruction.

```rust
// repair.rs — before
fn is_rewrite_subject(subject: &str) -> bool {
    subject.starts_with("rebase")
        || subject.starts_with("commit (amend)")
        || subject.starts_with("commit(amend)")
}
// Matches rebase (start), rebase (pick), rebase (squash), rebase (fixup),
// rebase (finish). The pair extractor processes all of them.
```

```rust
// repair.rs::is_rewrite_subject — after
fn is_rewrite_subject(subject: &str) -> bool {
    // Only the *terminal* events that produce a user-visible new SHA matter
    // for repair. `rebase -i` also emits `rebase (start|pick|squash|fixup)`
    // for each intermediate step; pairing those with the prior reflog entry
    // would build (intermediate, intermediate) pairs and — if a note happens
    // to be attached to an intermediate SHA — migrate it onto an unrelated
    // commit that prov gc would later cull.
    subject.starts_with("rebase (finish)")
        || subject.starts_with("commit (amend)")
        || subject.starts_with("commit(amend)")
}
```

The unit tests for `is_rewrite_subject` explicitly assert the negative cases:

```rust
assert!(!is_rewrite_subject("rebase (start): checkout main"));
assert!(!is_rewrite_subject("rebase (pick): foo"));
assert!(!is_rewrite_subject("rebase (squash): bar"));
```

The rule: when classifying git lifecycle events by reflog subject string, identify the full set of subjects the operation produces and decide which subset produces a durable, user-visible SHA worth acting on. Match only that subset. Add unit tests that explicitly assert non-terminal events are rejected.

### 5. Agent-callable CLI commands ship `--json` from day one

The existing read surface (`prov log`, `prov search`, `prov reindex`, `prov pr-timeline`) all emit versioned JSON envelopes via `--json`. The U9 write/admin commands `prov repair` and `prov gc` initially shipped without it.

An agent calling `prov repair` to recover from a bypassed hook can only check the exit code from a text-output command. It cannot distinguish "migrated 3 pairs" from "migrated 0 pairs but exited 0 because nothing was orphaned". Retrofitting `--json` later forces the future Skill (U12) to do regex scraping during the gap, then switch formats — a maintenance cost and an error surface.

Both commands now emit structured envelopes:
- `prov gc --json`: `{ culled_public, culled_private, pruned_sessions, compacted, dry_run, prov_version }`
- `prov repair --json`: `{ migrated_public, migrated_private, pairs: [{ old, new, ref_name, status }], days_walked, ref_walked, dry_run, prov_version }`

The `status` field on each pair uses a closed vocabulary of string literals (`migrated`, `would-migrate`, `skipped-existing`, `skipped-no-source`, `failed`) so agents can branch on status without parsing free text.

The companion `git-subprocess-hardening-conventions-2026-05-02.md` covers the broader agent-safety surface. This entry adds: the `--json` requirement applies to **write and admin commands**, not only read commands. Any prov subcommand that modifies git state, prunes data, or migrates notes must ship `--json` at the same time it ships its first public interface.

## Why This Matters

Each of these five patterns produces a failure that is **silent by default**:

- **Wrong-polarity prune fallback** (`gc.rs::prune_staging`): sessions accumulate forever; no error, no warning, disk quietly fills. The user only notices when `prov gc` appears to do nothing.
- **Wrong-polarity Err coercion** (`common.rs::invalidate_cache_per_sha`): a transient `git rev-parse` failure during heavy repo activity wipes the cache stamp; every subsequent `prov log` pays a full cold-reindex cost. No error visible to the user; only a performance regression they cannot diagnose.
- **Reachability blind spot** (`gc.rs::is_reachable`): the note for a WIP commit on detached HEAD is culled silently. The user does not notice until they `git checkout` back to a branch and find the provenance missing.
- **Nullable dedupe key collapse** (`hook.rs::dedupe_and_sort_edits`): a squash of three commits loses two-thirds of the recorded edits. The note file looks valid; it just has fewer entries than it should. The user cannot recover the lost edits.
- **Over-broad reflog classification** (`repair.rs::is_rewrite_subject`): `prov repair` migrates a note to the wrong commit; `prov gc` culls it on the next run. Two silent steps with no signal at either step.

Silent data loss is the worst failure class in a provenance tool. The entire value proposition is that the record is there when the user needs it. If the record is gone and no error was ever shown, the user cannot know whether the tool failed, whether they ran the wrong command, or whether the data was never captured. Loud failures — errors, non-zero exits, explicit "no matches" output — are recoverable. Silent losses are not.

## When to Apply

Apply these conventions whenever writing or reviewing:

- Any helper that uses `unwrap_or`, `unwrap_or_else`, `.ok()`, or `.ok().flatten()` on a result that controls a prune, cull, skip, or write decision.
- Any dedupe key struct or tuple that contains an `Option<_>` field.
- Any function that answers "is this commit/object reachable?" or "is this SHA in this set?" by delegating to a single git command.
- Any function that classifies git events by matching reflog subject strings, especially for rebase, amend, or merge operations.
- Any new prov subcommand that writes to git state, prunes data, migrates notes, or performs housekeeping. All such commands must include `--json` at their initial release.

In practice: `gc.rs`, `repair.rs`, `hook.rs`, and any future hook handler or housekeeping command are the primary application sites.

## Examples

All examples are from PR #32 (`feat/u9-history-rewrite`), fix commit `8f27480`.

- **`crates/prov-cli/src/commands/gc.rs`** (`prune_staging`): `unwrap_or(UNIX_EPOCH)` replaces `unwrap_or_else(SystemTime::now)`. Comment: "An empty/unreadable session dir has no mtime to anchor on. Treat it as maximally stale (UNIX_EPOCH) so it falls past any cutoff and gets pruned, instead of falling back to `now()` and surviving forever."
- **`crates/prov-cli/src/commands/common.rs`** (`invalidate_cache_per_sha`): `if let Ok(Some(sha))` guards the `set_recorded_notes_ref_sha` call. The Err arm is empty, leaving the prior stamp in place.
- **`crates/prov-cli/src/commands/hook.rs`** (`dedupe_and_sort_edits`): key type is `(String, u32, Option<String>, String)`. The fourth element is `format!("{}@{}-{}", e.file, e.line_range[0], e.line_range[1])` when `tool_use_id.is_none()`, and `String::new()` otherwise.
- **`crates/prov-cli/src/commands/gc.rs`** (`is_reachable`): three-step check — `cat-file -e` existence guard, then `for-each-ref --contains`, then `merge-base --is-ancestor <sha> HEAD`. Doc comment names the blind spot explicitly: "`git for-each-ref` does not consider HEAD as a starting point."
- **`crates/prov-cli/src/commands/repair.rs`** (`is_rewrite_subject`): matches only `"rebase (finish)"`, `"commit (amend)"`, and `"commit(amend)"`. Unit tests assert that `"rebase (start)"`, `"rebase (pick)"`, and `"rebase (squash)"` return false.
- **`crates/prov-cli/src/commands/{gc,repair}.rs`**: `--json` flags and their serialization structs (`GcJson`, `RepairJson`, `RepairPair`). `RepairPair.status` uses a closed vocabulary of five string literals documented inline.

Tests covering each pattern landed in the same commit:
- `cli_repair::gc_preserves_notes_for_detached_head_commits` — pattern 3.
- `cli_repair::gc_compact_skips_notes_with_empty_timestamps` and `gc_prunes_stale_staging_sessions` — pattern 1.
- `cli_rewrite::squash_with_none_tool_use_id_keeps_distinct_file_regions` — pattern 2.
- `cli_rewrite::rewrite_no_op_pair_preserves_note` — defensive-guard test.
- `repair::tests::rewrite_subjects_classified` — pattern 4 (with negative assertions).

## Related

- `docs/solutions/conventions/git-subprocess-hardening-conventions-2026-05-02.md` — companion conventions doc covering the layer below: defensive patterns for shelling out to git (argument injection, terminal prompts, ls-tree validation, hint-string accuracy). Same module, sibling category — both should be consulted when working in `crates/prov-cli/`.
- Issue #25 — `# prov:private` routing falls through to public on same-second timestamp tie. Same defensive-default polarity theme: "prefer private on tie" matches the polarity reasoning in pattern 1.
- Issue #29 — `redact-history` leaves pre-rewrite content reachable via local reflog and unpruned blobs. Connects to the reachability and prune themes; the prune-cutoff polarity convention here is part of why a built-in `prov gc-secrets` (or similar) needs careful default selection.
- Issue #30 — SQLite cache schema lacks `ref_name`; public + private notes for one commit silently collapse. Direct application of pattern 2 (dedupe keys with nullable/missing parts collapsing distinct values) at the storage layer.
