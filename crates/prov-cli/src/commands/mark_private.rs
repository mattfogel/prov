//! `prov mark-private <commit>` — retroactively move a commit's note from the
//! push-eligible public ref to the local-only private ref.
//!
//! Use case: the user shipped a note that contained sensitive context they
//! didn't realize was sensitive at the time. Running `mark-private` against
//! the commit pulls the note off `refs/notes/prompts` (the ref `prov push`
//! sends) and re-attaches it to `refs/notes/prompts-private` (which is never
//! pushed and is blocked by the U8 pre-push gate even when manually mapped).
//! Local reads still see the prompt because the resolver overlays both refs.
//!
//! Behaviour notes:
//! - Idempotent: running on a commit with no public note prints a clear
//!   message and exits 0. Running on a commit whose note already lives on the
//!   private ref is also a no-op.
//! - The cache is updated in place — the entry stays under the same
//!   `commit_sha`, so `prov log` finds it instantly without a reindex.
//! - **Caveat:** if the public ref was already pushed, this command does NOT
//!   scrub the remote. The user must re-push (force) the public ref AND
//!   rotate any leaked secret independently. We document this on the way out.

use anyhow::{anyhow, Context};
use clap::Parser;

use prov_core::git::{Git, GitError};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

use super::common::CACHE_FILENAME;

#[derive(Parser, Debug)]
pub struct Args {
    /// Commit SHA whose note should move to the local-only private ref.
    pub commit: String,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = Git::discover(&cwd).map_err(|e| match e {
        GitError::NotARepo => anyhow!("not in a git repo"),
        other => anyhow::Error::from(other),
    })?;

    // Resolve the commit-ish to a full SHA so users can pass `HEAD`, short
    // SHAs, or branch names without us guessing whether `git notes show` will
    // resolve them. A bad ref errors here with git's own message.
    // `--end-of-options` keeps a user-supplied value beginning with `-` from
    // being parsed as a git flag (e.g., `prov mark-private --version`).
    let resolved = git
        .capture(["rev-parse", "--verify", "--end-of-options", &args.commit])
        .map_err(|e| anyhow!("could not resolve commit `{}`: {e}", args.commit))?
        .trim()
        .to_string();

    let public = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    let private = NotesStore::new(git.clone(), NOTES_REF_PRIVATE);

    let Some(note) = public.read(&resolved)? else {
        // No public note. If a private note already exists, surface that —
        // common when the user runs the command twice. Otherwise, nothing to do.
        if private.read(&resolved)?.is_some() {
            println!("prov mark-private: {resolved} is already on the private ref");
        } else {
            println!("prov mark-private: no public note attached to {resolved}");
        }
        return Ok(());
    };

    // Copy first, remove second. If the write fails, the public note stays
    // in place — better to leak again than to delete the only copy.
    private.write(&resolved, &note)?;
    public.remove(&resolved)?;

    // Refresh the cache. The note's content is unchanged, so the cache row
    // can stay as-is, but `cache_meta.notes_ref_sha` now disagrees with the
    // public ref (it shrank). Drop and re-upsert under the no-stamp helper so
    // freshness tracks the public ref's new state on the next reindex.
    let cache_path = git.git_dir().join(CACHE_FILENAME);
    if cache_path.exists() {
        if let Ok(mut cache) = Cache::open(&cache_path) {
            // delete_note removes from the cache; upsert_note_no_stamp re-adds
            // the same content but without touching the public ref's SHA stamp.
            // Net effect: identical row content, public-ref-SHA stays stale
            // until next reindex (acceptable — drift detection is a soft warn).
            cache.delete_note(&resolved).ok();
            cache.upsert_note_no_stamp(&resolved, &note).ok();
        }
    }

    println!("prov mark-private: moved note for {resolved} to {NOTES_REF_PRIVATE}");
    println!(
        "  note: if {NOTES_REF_PUBLIC} was already pushed, the remote still has the old content."
    );
    println!(
        "  rotate any leaked secret and consider `prov redact-history` for retroactive scrub."
    );
    Ok(())
}
