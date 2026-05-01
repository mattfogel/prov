//! `prov reindex` — rebuild the SQLite cache from the public notes ref.
//!
//! Run after `git fetch refs/notes/prompts:...` or any external `git notes`
//! write. Drops the cache tables and repopulates them from `NotesStore::list`.
//! Records the source notes-ref SHA in `cache_meta` so future reads can detect
//! drift via `Resolver::ensure_fresh`.

use clap::Parser;
use serde::Serialize;

use super::common::RepoHandles;

#[derive(Parser, Debug)]
pub struct Args {
    /// Emit JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let mut handles = RepoHandles::open()?;
    let public_stats = handles.cache.reindex_from(&handles.notes)?;
    // Layer private notes on top so local reads see them without pushing.
    let private_stats = handles.cache.overlay_from(&handles.private_notes)?;
    let total_notes = public_stats.notes + private_stats.notes;
    let total_edits = public_stats.edits + private_stats.edits;

    if args.json {
        let payload = ReindexJson {
            notes: total_notes,
            edits: total_edits,
            cache_path: handles.cache_path.display().to_string(),
            prov_version: env!("CARGO_PKG_VERSION"),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if total_notes == 0 {
        println!("no notes to index");
    } else {
        println!(
            "reindexed {total_notes} note(s), {total_edits} edit(s) into {}",
            handles.cache_path.display()
        );
    }
    Ok(())
}

#[derive(Serialize)]
struct ReindexJson {
    notes: u32,
    edits: u32,
    cache_path: String,
    prov_version: &'static str,
}
