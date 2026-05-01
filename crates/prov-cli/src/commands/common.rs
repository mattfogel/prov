//! Shared helpers for the read-side CLI commands.
//!
//! `log`, `search`, `reindex`, and `pr-timeline` all need the same setup:
//! discover the repo from cwd, open the public notes ref, open the SQLite
//! cache at `<git-dir>/prov.db`. Centralizing the wiring keeps the per-command
//! files thin and makes "not in a git repo" exit cleanly with code 1 across
//! all commands.

use std::path::PathBuf;

use anyhow::{anyhow, Context};

use prov_core::git::{Git, GitError};
use prov_core::resolver::Resolver;
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

/// Filename of the SQLite cache under `<git-dir>/`.
pub const CACHE_FILENAME: &str = "prov.db";

/// Bundle of read-side handles built from cwd. Carries a public and a private
/// `NotesStore`; reads layer the private ref over the public one so the user
/// sees their own `# prov:private` notes locally without ever pushing them.
pub struct RepoHandles {
    pub git: Git,
    pub notes: NotesStore,
    pub private_notes: NotesStore,
    pub cache: Cache,
    pub cache_path: PathBuf,
}

impl RepoHandles {
    /// Discover the repo from cwd, open both the public and private notes
    /// refs, and open (creating if needed) the cache file. Errors map to
    /// user-facing messages with exit code 1.
    pub fn open() -> anyhow::Result<Self> {
        let cwd = std::env::current_dir().context("could not read current directory")?;
        let git = match Git::discover(&cwd) {
            Ok(g) => g,
            Err(GitError::NotARepo) => {
                return Err(anyhow!("not in a git repo"));
            }
            Err(e) => return Err(e.into()),
        };
        let notes = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
        let private_notes = NotesStore::new(git.clone(), NOTES_REF_PRIVATE);
        let cache_path = git.git_dir().join(CACHE_FILENAME);
        let cache = Cache::open(&cache_path)
            .with_context(|| format!("failed to open prov cache at {}", cache_path.display()))?;
        Ok(Self {
            git,
            notes,
            private_notes,
            cache,
            cache_path,
        })
    }

    /// Consume self into a `Resolver`. The resolver owns the cache and notes
    /// store; the underlying `Git` handle is cheaply cloneable (PathBufs) so
    /// callers that still need it can grab it via `git.clone()` first.
    pub fn into_resolver(self) -> Resolver {
        Resolver::new(self.git, self.notes, self.cache)
    }
}
