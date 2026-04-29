//! `prov reindex` — rebuild the SQLite cache from the public notes ref.
//!
//! Run after `git fetch refs/notes/prompts:...` or any external `git notes`
//! write. Drops the cache tables and repopulates them from `NotesStore::list`.
//! Records the source notes-ref SHA in `cache_meta` so future reads can detect
//! drift via `Resolver::ensure_fresh`.

use clap::Parser;

use super::common::RepoHandles;

#[derive(Parser, Debug)]
pub struct Args {}

pub fn run(_args: Args) -> anyhow::Result<()> {
    let mut handles = RepoHandles::open()?;
    let stats = handles.cache.reindex_from(&handles.notes)?;
    if stats.notes == 0 {
        println!("no notes to index");
    } else {
        println!(
            "reindexed {} note(s), {} edit(s) into {}",
            stats.notes,
            stats.edits,
            handles.cache_path.display()
        );
    }
    Ok(())
}
