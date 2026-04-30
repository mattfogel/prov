//! Read and write JSON notes via `git notes`.
//!
//! `NotesStore` owns a [`Git`](crate::git::Git) plus a notes ref name. Reads
//! shell `git notes show`; writes shell `git notes add --force --file=-` with
//! the note JSON piped on stdin. All errors propagate as [`NotesError`].

use crate::git::{Git, GitError};
use crate::schema::{Note, SchemaError};

/// Read/write access to a single notes ref (typically `refs/notes/prompts`).
#[derive(Debug, Clone)]
pub struct NotesStore {
    git: Git,
    ref_name: String,
}

impl NotesStore {
    /// Construct a store bound to a repo and a notes ref.
    #[must_use]
    pub fn new(git: Git, ref_name: impl Into<String>) -> Self {
        Self {
            git,
            ref_name: ref_name.into(),
        }
    }

    /// Notes ref this store reads and writes.
    #[must_use]
    pub fn ref_name(&self) -> &str {
        &self.ref_name
    }

    /// Underlying git wrapper.
    #[must_use]
    pub fn git(&self) -> &Git {
        &self.git
    }

    /// Read the note for `commit_sha`. `Ok(None)` when no note is attached;
    /// errors only on git failures or schema-version mismatch.
    pub fn read(&self, commit_sha: &str) -> Result<Option<Note>, NotesError> {
        match self
            .git
            .capture(["notes", "--ref", &self.ref_name, "show", commit_sha])
        {
            Ok(json) => Ok(Some(Note::from_json(&json)?)),
            Err(GitError::CommandFailed { stderr, .. })
                if stderr.contains("no note found") || stderr.contains("No note") =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Write `note` to `commit_sha`. Replaces any existing note. The JSON is
    /// piped on stdin to avoid filesystem churn or argv-length limits.
    pub fn write(&self, commit_sha: &str, note: &Note) -> Result<(), NotesError> {
        let json = note.to_json()?;
        self.git.capture_with_stdin(
            [
                "notes",
                "--ref",
                &self.ref_name,
                "add",
                "--force",
                "--file=-",
                commit_sha,
            ],
            json.as_bytes(),
        )?;
        Ok(())
    }

    /// Remove the note attached to `commit_sha`. No-op if no note is attached.
    pub fn remove(&self, commit_sha: &str) -> Result<(), NotesError> {
        match self.git.run([
            "notes",
            "--ref",
            &self.ref_name,
            "remove",
            "--ignore-missing",
            commit_sha,
        ]) {
            Ok(()) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Iterate every `(commit_sha, Note)` pair currently in the notes ref.
    /// Returns an empty vec when the ref does not exist yet.
    pub fn list(&self) -> Result<Vec<(String, Note)>, NotesError> {
        let raw = match self.git.capture(["notes", "--ref", &self.ref_name, "list"]) {
            Ok(s) => s,
            // Empty-ref case: `git notes list` errors with "ref does not exist"
            // before any notes have been written.
            Err(GitError::CommandFailed { stderr, .. })
                if stderr.contains("Notes ref does not exist")
                    || stderr.contains("does not exist") =>
            {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e.into()),
        };

        let mut out = Vec::new();
        for line in raw.lines() {
            // `git notes list` prints `<note-blob-sha> <annotated-commit-sha>`.
            let mut parts = line.split_whitespace();
            let _note_sha = parts.next();
            let Some(commit_sha) = parts.next() else {
                continue;
            };
            if let Some(note) = self.read(commit_sha)? {
                out.push((commit_sha.to_string(), note));
            }
        }
        Ok(out)
    }

    /// Resolve the notes-ref SHA (i.e., `git rev-parse <ref>`). Useful for
    /// cache-coherency checks. Returns `Ok(None)` if the ref does not exist yet.
    pub fn ref_sha(&self) -> Result<Option<String>, NotesError> {
        match self
            .git
            .capture(["rev-parse", "--verify", "-q", &self.ref_name])
        {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(GitError::CommandFailed { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Errors raised by `NotesStore`.
#[derive(Debug, thiserror::Error)]
pub enum NotesError {
    /// Underlying git invocation failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Note JSON failed schema validation or deserialization.
    #[error(transparent)]
    Schema(#[from] SchemaError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Edit, Note};
    use std::process::Command;
    use tempfile::TempDir;

    fn fixture_repo() -> (TempDir, Git, String) {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        for args in [
            vec!["init", "-q", "-b", "main", "."],
            vec!["config", "--local", "user.email", "t@x.com"],
            vec!["config", "--local", "user.name", "T"],
        ] {
            assert!(Command::new("git")
                .current_dir(path)
                .args(&args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(path.join("README.md"), "hi").unwrap();
        for args in [vec!["add", "README.md"], vec!["commit", "-q", "-m", "init"]] {
            assert!(Command::new("git")
                .current_dir(path)
                .args(&args)
                .status()
                .unwrap()
                .success());
        }
        let sha = String::from_utf8(
            Command::new("git")
                .current_dir(path)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        let git = Git::discover(path).unwrap();
        (dir, git, sha)
    }

    fn sample_note() -> Note {
        Note::new(vec![Edit {
            file: "README.md".into(),
            line_range: [1, 1],
            content_hashes: vec!["abc".into()],
            original_blob_sha: Some("def".into()),
            prompt: "say hi".into(),
            conversation_id: "sess_1".into(),
            turn_index: 0,
            tool_use_id: None,
            preceding_turns_summary: None,
            model: "claude-sonnet-4-5".into(),
            tool: "claude-code".into(),
            timestamp: "2026-04-28T00:00:00Z".into(),
            derived_from: None,
        }])
    }

    #[test]
    fn write_then_read_roundtrips() {
        let (_dir, repo, sha) = fixture_repo();
        let store = NotesStore::new(repo, "refs/notes/prompts");
        let original = sample_note();
        store.write(&sha, &original).unwrap();
        let parsed = store.read(&sha).unwrap().expect("note present");
        assert_eq!(parsed, original);
    }

    #[test]
    fn read_missing_note_is_ok_none() {
        let (_dir, git, sha) = fixture_repo();
        let store = NotesStore::new(git, "refs/notes/prompts");
        assert!(store.read(&sha).unwrap().is_none());
    }

    #[test]
    fn list_returns_all_written_notes() {
        let (_dir, git, sha) = fixture_repo();
        let store = NotesStore::new(git, "refs/notes/prompts");
        store.write(&sha, &sample_note()).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, sha);
    }

    #[test]
    fn list_empty_ref_returns_empty_vec() {
        let (_dir, git, _sha) = fixture_repo();
        let store = NotesStore::new(git, "refs/notes/prompts");
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn remove_clears_note() {
        let (_dir, git, sha) = fixture_repo();
        let store = NotesStore::new(git, "refs/notes/prompts");
        store.write(&sha, &sample_note()).unwrap();
        store.remove(&sha).unwrap();
        assert!(store.read(&sha).unwrap().is_none());
    }

    #[test]
    fn remove_missing_is_idempotent() {
        let (_dir, git, sha) = fixture_repo();
        let store = NotesStore::new(git, "refs/notes/prompts");
        store.remove(&sha).unwrap();
        store.remove(&sha).unwrap();
    }

    #[test]
    fn ref_sha_returns_none_before_first_write() {
        let (_dir, git, _sha) = fixture_repo();
        let store = NotesStore::new(git, "refs/notes/prompts");
        assert!(store.ref_sha().unwrap().is_none());
    }

    #[test]
    fn ref_sha_returns_some_after_write() {
        let (_dir, git, sha) = fixture_repo();
        let store = NotesStore::new(git, "refs/notes/prompts");
        store.write(&sha, &sample_note()).unwrap();
        let ref_sha = store.ref_sha().unwrap().expect("ref exists");
        assert_eq!(ref_sha.len(), 40);
    }
}
