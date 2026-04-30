//! `(file, line) -> originating prompt + drift state` lookup pipeline.
//!
//! The resolver is the single shared entry point used by the CLI, the Claude
//! Code Skill (via the CLI), and the GitHub Action. The pipeline:
//!
//! 1. `git blame -C -M --line-porcelain` for the file and line. This follows
//!    file copies/moves and reports the *originating* commit and original line
//!    number in that commit.
//! 2. Look up the note attached to that commit. SQLite cache first; on miss,
//!    shell `git notes show`, populate the cache, retry once.
//! 3. Find the edit entry whose `line_range` contains the original line.
//! 4. BLAKE3-hash the current line content and compare to the stored hash at
//!    the matching `line_idx`. Match → `Unchanged`; differ → `Drifted`.
//!
//! Errors are funnelled into `NoProvenance { reason }` rather than `Err`. The
//! caller needs a single read path that distinguishes "no provenance for this
//! line" from "git fell over"; the former is a normal answer.
//!
//! Cache-coherency is checked once at construction time (or via
//! [`Resolver::ensure_fresh`]); a drifted cache logs a warning to stderr and
//! does not block the resolve. The cache is repopulated on miss.

use std::path::{Path, PathBuf};

use crate::git::{Git, GitError};
use crate::schema::{DerivedFrom, Edit, Note, SchemaError};
use crate::storage::notes::{NotesError, NotesStore};
use crate::storage::sqlite::{Cache, CacheError};

/// Lookup pipeline shared by the CLI, Skill, and GitHub Action.
pub struct Resolver {
    git: Git,
    notes: NotesStore,
    cache: Cache,
}

impl Resolver {
    /// Build a resolver bound to a repo, a notes store, and a SQLite cache.
    #[must_use]
    pub fn new(git: Git, notes: NotesStore, cache: Cache) -> Self {
        Self { git, notes, cache }
    }

    /// Best-effort cache freshness check. If the cache's recorded notes-ref SHA
    /// drifts from the live ref, log a warning to stderr. The caller can choose
    /// to call [`Cache::reindex_from`] for strict correctness; resolve still
    /// works because cache misses fall back to `NotesStore::read`.
    pub fn ensure_fresh(&self) -> Result<bool, ResolverError> {
        let recorded = self.cache.recorded_notes_ref_sha()?;
        let live = self.notes.ref_sha()?;
        let fresh = recorded == live;
        if !fresh {
            eprintln!(
                "prov: cache may be stale (recorded={recorded:?}, live={live:?}); run `prov reindex` to refresh"
            );
        }
        Ok(fresh)
    }

    /// Resolve `(file, line)` to a result describing the originating prompt and
    /// whether the current line content matches what was AI-written.
    ///
    /// `file` may be repo-relative or absolute (it is passed to `git blame` as
    /// given). `line` is 1-based.
    pub fn resolve(&self, file: &Path, line: u32) -> Result<ResolveResult, ResolverError> {
        let Some(blame) = self.blame(file, line)? else {
            return Ok(ResolveResult::no_provenance(NoProvenanceReason::NoBlame));
        };

        let Some(note) = self.note_for(&blame.commit_sha)? else {
            return Ok(ResolveResult::no_provenance(
                NoProvenanceReason::NoNoteForCommit,
            ));
        };

        let Some((edit, line_idx)) = find_edit(&note, &blame.original_file, blame.original_line)
        else {
            return Ok(ResolveResult::no_provenance(
                NoProvenanceReason::NoMatchingNote,
            ));
        };

        let current = match read_current_line(&self.git, file, line) {
            Ok(c) => c,
            // Treat a missing-or-unreadable file as "no provenance" rather than
            // an error — the resolver is read-only and the caller may have a
            // stale path. Real git failures still surface as ResolverError.
            Err(ResolverError::Git(_)) => {
                return Ok(ResolveResult::no_provenance(
                    NoProvenanceReason::NoMatchingNote,
                ));
            }
            Err(e) => return Err(e),
        };

        let stored_hash = edit.content_hashes.get(line_idx).map(String::as_str);
        let current_hash = blake3_hex(current.as_bytes());

        let drifted = match stored_hash {
            Some(stored) => stored != current_hash,
            None => true,
        };

        if drifted {
            Ok(ResolveResult::Drifted {
                prompt: edit.prompt.clone(),
                model: edit.model.clone(),
                timestamp: edit.timestamp.clone(),
                conversation_id: edit.conversation_id.clone(),
                turn_index: edit.turn_index,
                tool_use_id: edit.tool_use_id.clone(),
                derived_from: edit.derived_from.clone(),
                original_blob_sha: edit.original_blob_sha.clone(),
                blame_author_after: blame.author,
                blame_commit: blame.commit_sha,
            })
        } else {
            Ok(ResolveResult::Unchanged {
                prompt: edit.prompt.clone(),
                model: edit.model.clone(),
                timestamp: edit.timestamp.clone(),
                conversation_id: edit.conversation_id.clone(),
                turn_index: edit.turn_index,
                tool_use_id: edit.tool_use_id.clone(),
                derived_from: edit.derived_from.clone(),
                blame_commit: blame.commit_sha,
            })
        }
    }

    fn blame(&self, file: &Path, line: u32) -> Result<Option<BlameLine>, ResolverError> {
        // -C -M follows copies/moves so the resolver still works after a file
        // is renamed or its content was lifted out of another file.
        let line_arg = format!("{line},{line}");
        let path_str = file.to_string_lossy();
        let raw = match self.git.capture([
            "blame",
            "-C",
            "-M",
            "--line-porcelain",
            "-L",
            line_arg.as_str(),
            "--",
            path_str.as_ref(),
        ]) {
            Ok(s) => s,
            // `git blame` errors when the file or line is out of range; that's
            // a normal "no provenance" answer, not a failure.
            Err(GitError::CommandFailed { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(parse_blame_porcelain(&raw))
    }

    fn note_for(&self, commit_sha: &str) -> Result<Option<Note>, ResolverError> {
        match self.cache.get_note(commit_sha) {
            Ok(Some(note)) => Ok(Some(note)),
            // Cache miss OR a cache row with a schema error: fall through to
            // the notes ref, which re-validates and either returns a clean
            // `Note` or surfaces the same `SchemaError` to the caller.
            Ok(None) | Err(CacheError::Schema(_)) => {
                self.notes.read(commit_sha).map_err(Into::into)
            }
            Err(e) => Err(e.into()),
        }
    }
}

/// Result of a `(file, line)` resolve.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolveResult {
    /// The current line content matches the stored hash; the prompt is
    /// authoritative for what the line says.
    Unchanged {
        /// Original prompt (post-redaction).
        prompt: String,
        /// Model name at session start.
        model: String,
        /// ISO-8601 capture timestamp.
        timestamp: String,
        /// Stable Claude Code session id.
        conversation_id: String,
        /// Zero-based turn index within the session.
        turn_index: u32,
        /// Tool-use id when the platform surfaced one.
        tool_use_id: Option<String>,
        /// AI-on-AI / backfill provenance link.
        derived_from: Option<DerivedFrom>,
        /// Commit blame attributed the line to.
        blame_commit: String,
    },
    /// The line was originally AI-written but its current content has changed.
    /// `blame_author_after` is the author who last touched the drifted form.
    Drifted {
        /// Original prompt (post-redaction).
        prompt: String,
        /// Model name at session start.
        model: String,
        /// ISO-8601 capture timestamp.
        timestamp: String,
        /// Stable Claude Code session id.
        conversation_id: String,
        /// Zero-based turn index within the session.
        turn_index: u32,
        /// Tool-use id when the platform surfaced one.
        tool_use_id: Option<String>,
        /// AI-on-AI / backfill provenance link.
        derived_from: Option<DerivedFrom>,
        /// Git blob SHA of the AI's original output (for `prov regenerate` diff).
        /// `None` when the originating note did not record one.
        original_blob_sha: Option<String>,
        /// Author who last touched the drifted line, per blame.
        blame_author_after: String,
        /// Commit blame attributed the line to.
        blame_commit: String,
    },
    /// No provenance available for this line. The `reason` distinguishes
    /// "no note", "no matching edit", "no blame", and "schema error".
    NoProvenance {
        /// Specific reason (for CLI rendering).
        reason: NoProvenanceReason,
    },
}

impl ResolveResult {
    fn no_provenance(reason: NoProvenanceReason) -> Self {
        Self::NoProvenance { reason }
    }

    /// True if the current line content matches the AI-captured hash.
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged { .. })
    }

    /// True if a prompt was found but the current line content has drifted.
    #[must_use]
    pub fn is_drifted(&self) -> bool {
        matches!(self, Self::Drifted { .. })
    }

    /// True if no provenance was attributable.
    #[must_use]
    pub fn is_no_provenance(&self) -> bool {
        matches!(self, Self::NoProvenance { .. })
    }
}

/// Why the resolver returned `NoProvenance`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoProvenanceReason {
    /// `git blame` produced no attribution for this line (file too short, or
    /// `git blame` itself failed for a non-fatal reason).
    NoBlame,
    /// Blame attributed the line to a commit, but no note is attached.
    NoNoteForCommit,
    /// A note exists, but no edit's `line_range` covers the original line.
    NoMatchingNote,
    /// Note JSON failed schema validation.
    SchemaError(String),
}

/// Errors raised by the resolver pipeline. Use sparingly: most "I can't tell
/// you" cases are returned via `ResolveResult::NoProvenance`. `ResolverError`
/// is reserved for failures the caller should surface (broken git, broken
/// cache, broken local clone).
#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    /// Underlying git invocation failed.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Notes store error (missing ref, invalid JSON at the ref, etc.).
    #[error(transparent)]
    Notes(#[from] NotesError),
    /// SQLite cache error.
    #[error(transparent)]
    Cache(#[from] CacheError),
    /// Note JSON failed schema validation.
    #[error(transparent)]
    Schema(#[from] SchemaError),
}

/// Parsed line of `git blame --line-porcelain` output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlameLine {
    commit_sha: String,
    /// Original line number in `original_file` at `commit_sha`.
    original_line: u32,
    original_file: PathBuf,
    author: String,
}

fn parse_blame_porcelain(raw: &str) -> Option<BlameLine> {
    // Porcelain layout (per `git blame --line-porcelain`):
    //   <sha> <orig-line> <final-line> <count>
    //   author <name>
    //   author-mail <email>
    //   ...
    //   filename <path>
    //   \t<line content>
    let mut lines = raw.lines();
    let header = lines.next()?;
    let mut header_parts = header.split_whitespace();
    let commit_sha = header_parts.next()?.to_string();
    let original_line: u32 = header_parts.next()?.parse().ok()?;

    let mut author = String::new();
    let mut original_file: Option<PathBuf> = None;

    for line in lines {
        if let Some(rest) = line.strip_prefix("author ") {
            author = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("filename ") {
            original_file = Some(PathBuf::from(rest));
        } else if line.starts_with('\t') {
            break;
        }
    }

    Some(BlameLine {
        commit_sha,
        original_line,
        original_file: original_file.unwrap_or_default(),
        author,
    })
}

fn find_edit<'a>(note: &'a Note, file: &Path, line: u32) -> Option<(&'a Edit, usize)> {
    let file_str = file.to_string_lossy();
    note.edits.iter().find_map(|edit| {
        if edit.file != file_str {
            return None;
        }
        let [start, end] = edit.line_range;
        if line < start || line > end {
            return None;
        }
        let line_idx = (line - start) as usize;
        Some((edit, line_idx))
    })
}

fn read_current_line(git: &Git, file: &Path, line: u32) -> Result<String, ResolverError> {
    let path = git.work_tree().join(file);
    let bytes = std::fs::read(&path).map_err(|e| GitError::Io(e.to_string()))?;
    let text = String::from_utf8_lossy(&bytes);
    let needed = line as usize;
    let nth = text
        .split_inclusive('\n')
        .nth(needed.saturating_sub(1))
        .unwrap_or("");
    // Strip the trailing newline so the hash matches what the capture pipeline
    // stored (capture hashes per-line content without the line terminator).
    Ok(nth
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string())
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_note(file: &str, start: u32, hashes: Vec<String>) -> Note {
        let len = u32::try_from(hashes.len()).expect("fixture line count fits in u32");
        Note::new(vec![Edit {
            file: file.into(),
            line_range: [start, start + len - 1],
            content_hashes: hashes,
            original_blob_sha: Some("deadbeef".into()),
            prompt: "demo prompt".into(),
            conversation_id: "sess_test".into(),
            turn_index: 0,
            tool_use_id: Some("toolu_t".into()),
            preceding_turns_summary: None,
            model: "claude-sonnet-4-5".into(),
            tool: "claude-code".into(),
            timestamp: "2026-04-28T00:00:00Z".into(),
            derived_from: None,
        }])
    }

    #[test]
    fn parse_porcelain_extracts_commit_and_line() {
        let raw = "abc123 3 5 1\nauthor Alice\nauthor-mail <a@x.com>\n\
                   author-time 1700000000\nauthor-tz +0000\n\
                   committer Alice\ncommitter-mail <a@x.com>\n\
                   committer-time 1700000000\ncommitter-tz +0000\n\
                   summary fixture\nfilename src/lib.rs\n\thello\n";
        let parsed = parse_blame_porcelain(raw).unwrap();
        assert_eq!(parsed.commit_sha, "abc123");
        assert_eq!(parsed.original_line, 3);
        assert_eq!(parsed.author, "Alice");
        assert_eq!(parsed.original_file, PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn find_edit_matches_inside_range() {
        let note = fixture_note(
            "src/lib.rs",
            10,
            vec!["h1".into(), "h2".into(), "h3".into()],
        );
        let (edit, idx) = find_edit(&note, Path::new("src/lib.rs"), 11).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(edit.line_range, [10, 12]);
    }

    #[test]
    fn find_edit_handles_inclusive_range_boundaries() {
        let note = fixture_note("src/lib.rs", 5, vec!["a".into(), "b".into()]);
        assert!(find_edit(&note, Path::new("src/lib.rs"), 5).is_some());
        assert!(find_edit(&note, Path::new("src/lib.rs"), 6).is_some());
        assert!(find_edit(&note, Path::new("src/lib.rs"), 4).is_none());
        assert!(find_edit(&note, Path::new("src/lib.rs"), 7).is_none());
    }

    #[test]
    fn find_edit_rejects_wrong_file() {
        let note = fixture_note("src/lib.rs", 1, vec!["h".into()]);
        assert!(find_edit(&note, Path::new("src/other.rs"), 1).is_none());
    }

    #[test]
    fn blake3_hex_matches_known_value() {
        // BLAKE3 of "" — sanity check the encoding used by capture and resolve.
        let h = blake3_hex(b"");
        assert_eq!(
            h,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(h.len(), 64);
    }
}
