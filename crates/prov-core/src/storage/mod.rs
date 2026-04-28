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

/// Default git ref where prov stores public (push-eligible) notes.
pub const NOTES_REF_PUBLIC: &str = "refs/notes/prompts";

/// Git ref where prov stores opt-out private notes. `prov install` does NOT
/// add this ref to remote refspecs; the pre-push hook double-checks and blocks
/// any manual attempt to push it.
pub const NOTES_REF_PRIVATE: &str = "refs/notes/prompts-private";
