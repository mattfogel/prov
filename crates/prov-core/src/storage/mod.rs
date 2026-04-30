//! Two-layer storage: durable git notes + indexed SQLite cache.
//!
//! - [`notes`] reads and writes JSON notes under `refs/notes/prompts` via
//!   shelling to `git notes`. Single source of truth.
//! - [`sqlite`] derives a queryable cache at `.git/prov.db` from the notes
//!   ref. Rebuildable from notes via `Cache::reindex_from`. Stores a
//!   `cache_meta` row tracking the notes-ref SHA at last reindex so reads
//!   can detect drift after `prov fetch` or external `git notes` writes.

pub mod notes;
pub mod sqlite;
pub mod staging;

/// Default git ref where prov stores public (push-eligible) notes.
pub const NOTES_REF_PUBLIC: &str = "refs/notes/prompts";

// The opt-out private notes ref will live at `refs/notes/prompts-private`
// once U7/U8 ship the routing + pre-push gate. Keeping the literal off the
// public surface until then so a stray import doesn't accidentally try to
// push it. See: docs/plans/.../v1-plan.md (U7, U8).
