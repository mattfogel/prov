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

/// Local-only notes ref for `# prov:private` opt-out content. `prov install`
/// never adds this to a remote refspec; the U8 pre-push gate also blocks any
/// push that names this ref locally even when manually mapped to a different
/// remote ref. Reads (resolver, `prov log`) overlay private notes on top of
/// the public ref, so the user still sees their own provenance locally.
pub const NOTES_REF_PRIVATE: &str = "refs/notes/prompts-private";
