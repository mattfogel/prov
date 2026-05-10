//! In-flight session state held under `.git/prov-staging/`.
//!
//! Capture is a multi-process state machine: every supported agent harness
//! fires its own short-lived `prov hook` invocation, accumulates one record
//! into the staging tree, and exits. The post-commit hook walks the tree,
//! matches staged edits against the commit's diff, and flushes matches into a
//! note.
//!
//! Layout (`<git-dir>/prov-staging/`):
//!
//! ```text
//! prov-staging/
//! ├── log                        # append-only diagnostic log (mode 0600)
//! └── <session-id>/              # per-session dir (mode 0700)
//!     ├── session.json           # SessionStart payload (model, turns)
//!     ├── turn-<N>.json          # one per UserPromptSubmit/Stop pair
//!     ├── edits.jsonl            # append-only PostToolUse records
//!     └── private/               # parallel layout for `# prov:private` turns
//!         ├── turn-<N>.json
//!         └── edits.jsonl
//! ```
//!
//! All files are written with mode 0600 and dirs with mode 0700, regardless of
//! the user's umask. Capture frequently runs in shared dev environments
//! (Codespaces, multi-user boxes) where world-readable staging would leak
//! pre-redaction prompt context to every other process owner.
//!
//! Append-only JSONL (one JSON record per line) is used for `edits.jsonl` and
//! the diagnostic `log`. Turn metadata uses atomic write-and-rename so a
//! killed-mid-write hook leaves at most a `.tmp` to garbage-collect, never a
//! truncated `turn-<N>.json`.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::SessionId;

/// Top-level staging directory name (placed under `.git/`).
pub const STAGING_DIRNAME: &str = "prov-staging";

/// Diagnostic-log filename inside the staging tree.
pub const LOG_FILENAME: &str = "log";

/// Per-session subdirectory carrying the `# prov:private` opt-out content.
pub const PRIVATE_SUBDIR: &str = "private";

/// Append-only JSONL file holding edit records produced by `PostToolUse`.
pub const EDITS_FILENAME: &str = "edits.jsonl";

/// Per-session metadata file written on `SessionStart`.
pub const SESSION_FILENAME: &str = "session.json";

/// Owned handle on the per-repo staging tree. Methods are best-effort: most
/// hook subcommands are required by the plan to exit 0 even on internal
/// error, so callers map [`StagingError`] to a log-and-continue policy.
#[derive(Debug, Clone)]
pub struct Staging {
    root: PathBuf,
}

impl Staging {
    /// Bind to `<git-dir>/prov-staging/` (creating it on demand).
    pub fn new(git_dir: &Path) -> Self {
        Self {
            root: git_dir.join(STAGING_DIRNAME),
        }
    }

    /// Bind to a custom root (used by tests).
    #[must_use]
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    /// Absolute path of the staging-root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Per-session directory path. Does NOT create it; pair with
    /// [`Staging::ensure_session_dir`] when you need it on disk.
    pub fn session_dir(&self, sid: &SessionId, private: bool) -> PathBuf {
        let mut p = self.root.join(sid.as_str());
        if private {
            p.push(PRIVATE_SUBDIR);
        }
        p
    }

    /// Ensure the per-session directory (and the staging root) exist with
    /// mode 0700. Idempotent.
    pub fn ensure_session_dir(
        &self,
        sid: &SessionId,
        private: bool,
    ) -> Result<PathBuf, StagingError> {
        ensure_dir_0700(&self.root)?;
        let session = self.root.join(sid.as_str());
        ensure_dir_0700(&session)?;
        if private {
            let priv_dir = session.join(PRIVATE_SUBDIR);
            ensure_dir_0700(&priv_dir)?;
            Ok(priv_dir)
        } else {
            Ok(session)
        }
    }

    /// Count the existing `turn-<N>.json` files in the (regular) session dir.
    /// New turns use `count_turns()` as their index.
    ///
    /// Counts only the regular dir (non-private). Private turns are stored in
    /// the parallel `<sid>/private/` subtree and indexed independently — a
    /// public turn index N never collides with a private turn index N because
    /// the dirs are disjoint.
    pub fn count_turns(&self, sid: &SessionId, private: bool) -> Result<u32, StagingError> {
        let dir = self.session_dir(sid, private);
        if !dir.exists() {
            return Ok(0);
        }
        let mut n = 0_u32;
        for entry in fs::read_dir(&dir).map_err(|e| io(&dir, &e))? {
            let entry = entry.map_err(|e| io(&dir, &e))?;
            let name = entry.file_name();
            let Some(s) = name.to_str() else { continue };
            // Suffix check is case-sensitive intentionally: prov writes
            // `.json` (lowercase) and only counts what it wrote.
            #[allow(clippy::case_sensitive_file_extension_comparisons)]
            let is_turn = s.starts_with("turn-") && s.ends_with(".json");
            if is_turn {
                n = n.saturating_add(1);
            }
        }
        Ok(n)
    }

    /// Atomically write a turn record at `turn-<idx>.json`.
    ///
    /// Writes to `turn-<idx>.json.tmp` (mode 0600) and renames into place. A
    /// killed-mid-write hook leaves the `.tmp` for `prov gc` to clean.
    pub fn write_turn(
        &self,
        sid: &SessionId,
        private: bool,
        idx: u32,
        record: &TurnRecord,
    ) -> Result<PathBuf, StagingError> {
        let dir = self.ensure_session_dir(sid, private)?;
        let final_path = dir.join(format!("turn-{idx}.json"));
        let tmp_path = dir.join(format!("turn-{idx}.json.tmp"));
        let json = serde_json::to_vec_pretty(record).map_err(StagingError::Serde)?;
        write_0600(&tmp_path, &json)?;
        fs::rename(&tmp_path, &final_path).map_err(|e| io(&final_path, &e))?;
        Ok(final_path)
    }

    /// Read every `turn-<N>.json` in the (regular) session dir, in ascending
    /// `<N>` order, skipping malformed files (defensive — capture-side
    /// failures must not stop the post-commit flush).
    pub fn read_turns(
        &self,
        sid: &SessionId,
        private: bool,
    ) -> Result<Vec<TurnRecord>, StagingError> {
        let dir = self.session_dir(sid, private);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut indexed: Vec<(u32, PathBuf)> = Vec::new();
        for entry in fs::read_dir(&dir).map_err(|e| io(&dir, &e))? {
            let entry = entry.map_err(|e| io(&dir, &e))?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(num) = name
                .strip_prefix("turn-")
                .and_then(|s| s.strip_suffix(".json"))
            else {
                continue;
            };
            if let Ok(n) = num.parse::<u32>() {
                indexed.push((n, path));
            }
        }
        indexed.sort_by_key(|(n, _)| *n);

        let mut out = Vec::with_capacity(indexed.len());
        for (_, path) in indexed {
            let Ok(bytes) = fs::read(&path) else {
                continue; // skip unreadable files defensively
            };
            if let Ok(rec) = serde_json::from_slice::<TurnRecord>(&bytes) {
                out.push(rec);
            }
            // malformed JSON is silently skipped; the corresponding edits in
            // edits.jsonl can still be matched against the commit diff.
        }
        Ok(out)
    }

    /// Append one edit record to `<session>/edits.jsonl`.
    ///
    /// JSONL is append-only by design: `O_APPEND` writes are atomic up to
    /// `PIPE_BUF` on POSIX, so two concurrent agent sessions writing to
    /// different files do not interfere. Within one session each harness
    /// serializes hook lifecycle events.
    pub fn append_edit(
        &self,
        sid: &SessionId,
        private: bool,
        record: &EditRecord,
    ) -> Result<(), StagingError> {
        let dir = self.ensure_session_dir(sid, private)?;
        let path = dir.join(EDITS_FILENAME);
        let mut json = serde_json::to_vec(record).map_err(StagingError::Serde)?;
        json.push(b'\n');
        append_0600(&path, &json)
    }

    /// Read every edit record in the session's `edits.jsonl`, skipping
    /// malformed lines defensively.
    pub fn read_edits(
        &self,
        sid: &SessionId,
        private: bool,
    ) -> Result<Vec<EditRecord>, StagingError> {
        let path = self.session_dir(sid, private).join(EDITS_FILENAME);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let f = fs::File::open(&path).map_err(|e| io(&path, &e))?;
        let reader = BufReader::new(f);
        let mut out = Vec::new();
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(rec) = serde_json::from_str::<EditRecord>(&line) {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// Write the session metadata (model, etc.) atomically. Overwrites if
    /// already present.
    pub fn write_session_meta(
        &self,
        sid: &SessionId,
        meta: &SessionMeta,
    ) -> Result<PathBuf, StagingError> {
        let dir = self.ensure_session_dir(sid, false)?;
        let final_path = dir.join(SESSION_FILENAME);
        let tmp_path = dir.join(format!("{SESSION_FILENAME}.tmp"));
        let json = serde_json::to_vec_pretty(meta).map_err(StagingError::Serde)?;
        write_0600(&tmp_path, &json)?;
        fs::rename(&tmp_path, &final_path).map_err(|e| io(&final_path, &e))?;
        Ok(final_path)
    }

    /// Read session metadata, returning `Ok(None)` if not yet written.
    pub fn read_session_meta(&self, sid: &SessionId) -> Result<Option<SessionMeta>, StagingError> {
        let path = self.session_dir(sid, false).join(SESSION_FILENAME);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).map_err(|e| io(&path, &e))?;
        match serde_json::from_slice::<SessionMeta>(&bytes) {
            Ok(m) => Ok(Some(m)),
            Err(_) => Ok(None),
        }
    }

    /// Append one line to `<staging>/log`. Used by hook subcommands to record
    /// failures without blocking the agent loop or commit.
    pub fn append_log(&self, line: &str) -> Result<(), StagingError> {
        ensure_dir_0700(&self.root)?;
        let path = self.root.join(LOG_FILENAME);
        let mut bytes = line.as_bytes().to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        append_0600(&path, &bytes)
    }

    /// List every session id currently present in staging.
    pub fn list_sessions(&self) -> Result<Vec<SessionId>, StagingError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|e| io(&self.root, &e))? {
            let entry = entry.map_err(|e| io(&self.root, &e))?;
            if !entry
                .file_type()
                .map_err(|e| io(&entry.path(), &e))?
                .is_dir()
            {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if let Ok(sid) = SessionId::parse(name.to_string()) {
                    out.push(sid);
                }
            }
        }
        Ok(out)
    }

    /// Remove the per-session staging directory after a successful flush.
    pub fn remove_session(&self, sid: &SessionId) -> Result<(), StagingError> {
        let dir = self.session_dir(sid, false);
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| io(&dir, &e))?;
        }
        Ok(())
    }
}

/// Per-turn record written by `UserPromptSubmit` and updated by `Stop`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnRecord {
    /// Stable session id this turn belongs to.
    pub session_id: String,
    /// Zero-based turn index within the session.
    pub turn_index: u32,
    /// Post-redaction prompt text. Even staged content is scrubbed write-time.
    pub prompt: String,
    /// True when the turn was marked `# prov:private` (case-insensitive,
    /// first/last line of the prompt only). Routes the turn to the
    /// `private/` subtree so it never reaches `refs/notes/prompts`.
    pub private: bool,
    /// `transcript_path` from the hook payload, when surfaced. Lets
    /// `prov backfill` find historical context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    /// Working directory the hook was invoked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// ISO-8601 timestamp when the turn was staged.
    pub started_at: String,
    /// ISO-8601 timestamp when `Stop` finalized the turn (None until then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// One PostToolUse edit record.
///
/// Field set is deliberately closer to the eventual `schema::Edit` so the
/// post-commit flush can match-and-promote without reshaping. The capture-side
/// representation does carry a few extra fields (`tool_name`, `before`,
/// `after`) that the eventual `Edit` does not store — they're only needed to
/// run the diff-matching strategies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditRecord {
    /// Stable session id this edit belongs to.
    pub session_id: String,
    /// Zero-based turn index within the session.
    pub turn_index: u32,
    /// Tool that produced the edit (`Edit` | `Write` | `MultiEdit`).
    pub tool_name: String,
    /// Agent harness that produced the edit (`claude-code`, `codex`, etc.).
    #[serde(default = "default_edit_tool")]
    pub tool: String,
    /// Per-tool-call correlation handle when the platform surfaces one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Repo-relative file path edited.
    pub file: String,
    /// Inclusive `[start, end]` line range in the post-edit content.
    pub line_range: [u32; 2],
    /// Pre-edit content fragment (from `tool_input` or reconstructed).
    pub before: String,
    /// Post-edit content fragment.
    pub after: String,
    /// BLAKE3 hash of each line in `after`, parallel to `line_range`.
    pub content_hashes: Vec<String>,
    /// Model that produced this edit, read from the transcript at capture
    /// time. `None` for legacy records (and as a defensive fallback when the
    /// transcript can't be read); the post-commit flush falls back to
    /// `SessionMeta.model` in that case. Per-edit capture is required because
    /// `SessionStart` only fires once per session, so a `/model` switch
    /// mid-session would otherwise mis-attribute every later turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// ISO-8601 timestamp.
    pub timestamp: String,
}

fn default_edit_tool() -> String {
    "claude-code".to_string()
}

/// `SessionStart` metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMeta {
    /// Stable session id.
    pub session_id: String,
    /// Model name as reported on `SessionStart` (e.g., `claude-sonnet-4-5`).
    pub model: String,
    /// ISO-8601 timestamp when the session started.
    pub started_at: String,
}

/// Errors raised by staging operations.
#[derive(Debug, thiserror::Error)]
pub enum StagingError {
    /// Filesystem I/O failure (with the offending path attached).
    #[error("staging I/O at {path}: {source}")]
    Io {
        /// Path the operation was attempting.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// JSON serialization or deserialization failure.
    #[error("staging serde: {0}")]
    Serde(#[source] serde_json::Error),
}

fn io(path: &Path, e: &std::io::Error) -> StagingError {
    StagingError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(e.kind(), e.to_string()),
    }
}

#[cfg(unix)]
fn ensure_dir_0700(path: &Path) -> Result<(), StagingError> {
    use std::os::unix::fs::DirBuilderExt;
    // Use `symlink_metadata` so a symlinked staging dir is rejected rather
    // than silently followed. A pre-planted symlink could otherwise redirect
    // mode-0700 writes onto a world-readable directory the attacker controls.
    match fs::symlink_metadata(path) {
        Ok(md) => {
            if md.file_type().is_symlink() {
                return Err(StagingError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "refusing to operate through a symlink",
                    ),
                });
            }
            if !md.is_dir() {
                return Err(StagingError::Io {
                    path: path.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "path exists and is not a directory",
                    ),
                });
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| io(path, &e)),
        Err(e) => Err(io(path, &e)),
    }
}

#[cfg(not(unix))]
fn ensure_dir_0700(path: &Path) -> Result<(), StagingError> {
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(path).map_err(|e| io(path, &e))
}

#[cfg(unix)]
fn write_0600(path: &Path, bytes: &[u8]) -> Result<(), StagingError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| io(path, &e))?;
    f.write_all(bytes).map_err(|e| io(path, &e))
}

#[cfg(not(unix))]
fn write_0600(path: &Path, bytes: &[u8]) -> Result<(), StagingError> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| io(path, &e))?;
    f.write_all(bytes).map_err(|e| io(path, &e))
}

#[cfg(unix)]
fn append_0600(path: &Path, bytes: &[u8]) -> Result<(), StagingError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| io(path, &e))?;
    f.write_all(bytes).map_err(|e| io(path, &e))
}

#[cfg(not(unix))]
fn append_0600(path: &Path, bytes: &[u8]) -> Result<(), StagingError> {
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|e| io(path, &e))?;
    f.write_all(bytes).map_err(|e| io(path, &e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Staging, SessionId) {
        let tmp = TempDir::new().unwrap();
        let staging = Staging::with_root(tmp.path().join("prov-staging"));
        let sid = SessionId::parse("sess_abc123").unwrap();
        (tmp, staging, sid)
    }

    fn turn(idx: u32, prompt: &str) -> TurnRecord {
        TurnRecord {
            session_id: "sess_abc123".into(),
            turn_index: idx,
            prompt: prompt.into(),
            private: false,
            transcript_path: None,
            cwd: None,
            started_at: "2026-04-28T12:00:00Z".into(),
            completed_at: None,
        }
    }

    fn edit(idx: u32, file: &str, line_start: u32) -> EditRecord {
        EditRecord {
            session_id: "sess_abc123".into(),
            turn_index: idx,
            tool_name: "Edit".into(),
            tool: "claude-code".into(),
            tool_use_id: Some("toolu_x".into()),
            file: file.into(),
            line_range: [line_start, line_start],
            before: String::new(),
            after: "hello\n".into(),
            content_hashes: vec![blake3::hash(b"hello").to_hex().to_string()],
            model: None,
            timestamp: "2026-04-28T12:00:01Z".into(),
        }
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn session_dir_layout_is_isolated() {
        let (_tmp, s, sid) = fixture();
        let pub_dir = s.ensure_session_dir(&sid, false).unwrap();
        let priv_dir = s.ensure_session_dir(&sid, true).unwrap();
        assert!(pub_dir.ends_with("sess_abc123"));
        assert!(priv_dir.ends_with(format!("sess_abc123/{PRIVATE_SUBDIR}")));
        assert!(pub_dir.exists());
        assert!(priv_dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn directories_are_mode_0700() {
        let (_tmp, s, sid) = fixture();
        s.ensure_session_dir(&sid, false).unwrap();
        assert_eq!(mode_of(s.root()), 0o700);
        assert_eq!(mode_of(&s.session_dir(&sid, false)), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn turn_files_are_mode_0600() {
        let (_tmp, s, sid) = fixture();
        let path = s.write_turn(&sid, false, 0, &turn(0, "hi")).unwrap();
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn count_turns_returns_zero_when_dir_missing() {
        let (_tmp, s, sid) = fixture();
        assert_eq!(s.count_turns(&sid, false).unwrap(), 0);
    }

    #[test]
    fn count_turns_after_writes() {
        let (_tmp, s, sid) = fixture();
        s.write_turn(&sid, false, 0, &turn(0, "first")).unwrap();
        s.write_turn(&sid, false, 1, &turn(1, "second")).unwrap();
        assert_eq!(s.count_turns(&sid, false).unwrap(), 2);
    }

    #[test]
    fn write_and_read_turns_roundtrips_in_order() {
        let (_tmp, s, sid) = fixture();
        s.write_turn(&sid, false, 0, &turn(0, "one")).unwrap();
        s.write_turn(&sid, false, 1, &turn(1, "two")).unwrap();
        s.write_turn(&sid, false, 2, &turn(2, "three")).unwrap();
        let read = s.read_turns(&sid, false).unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(read[0].turn_index, 0);
        assert_eq!(read[1].turn_index, 1);
        assert_eq!(read[2].turn_index, 2);
    }

    #[test]
    fn read_turns_skips_malformed_files() {
        let (_tmp, s, sid) = fixture();
        let dir = s.ensure_session_dir(&sid, false).unwrap();
        // Plant a malformed file.
        fs::write(dir.join("turn-0.json"), b"not json").unwrap();
        // And one valid one.
        s.write_turn(&sid, false, 1, &turn(1, "ok")).unwrap();
        let read = s.read_turns(&sid, false).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].turn_index, 1);
    }

    #[test]
    fn append_edit_creates_jsonl() {
        let (_tmp, s, sid) = fixture();
        s.append_edit(&sid, false, &edit(0, "src/lib.rs", 1))
            .unwrap();
        s.append_edit(&sid, false, &edit(0, "src/lib.rs", 2))
            .unwrap();
        let read = s.read_edits(&sid, false).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].line_range, [1, 1]);
        assert_eq!(read[1].line_range, [2, 2]);
    }

    #[test]
    fn read_edits_skips_malformed_lines() {
        let (_tmp, s, sid) = fixture();
        let dir = s.ensure_session_dir(&sid, false).unwrap();
        let path = dir.join(EDITS_FILENAME);
        let valid = serde_json::to_string(&edit(0, "src/lib.rs", 1)).unwrap();
        let body = format!("{valid}\nnot a json line\n{valid}\n");
        fs::write(path, body).unwrap();
        let read = s.read_edits(&sid, false).unwrap();
        assert_eq!(read.len(), 2);
    }

    #[test]
    fn private_and_public_turns_count_independently() {
        let (_tmp, s, sid) = fixture();
        s.write_turn(&sid, false, 0, &turn(0, "public")).unwrap();
        s.write_turn(&sid, true, 0, &turn(0, "private")).unwrap();
        assert_eq!(s.count_turns(&sid, false).unwrap(), 1);
        assert_eq!(s.count_turns(&sid, true).unwrap(), 1);
    }

    #[test]
    fn write_turn_atomic_rename_leaves_no_tmp_on_success() {
        let (_tmp, s, sid) = fixture();
        s.write_turn(&sid, false, 0, &turn(0, "x")).unwrap();
        let dir = s.session_dir(&sid, false);
        let mut entries: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        entries.sort();
        assert_eq!(entries, vec!["turn-0.json".to_string()]);
    }

    #[test]
    fn session_meta_roundtrip() {
        let (_tmp, s, sid) = fixture();
        let meta = SessionMeta {
            session_id: "sess_abc123".into(),
            model: "claude-sonnet-4-5".into(),
            started_at: "2026-04-28T12:00:00Z".into(),
        };
        s.write_session_meta(&sid, &meta).unwrap();
        let read = s.read_session_meta(&sid).unwrap().unwrap();
        assert_eq!(read, meta);
    }

    #[test]
    fn session_meta_returns_none_when_absent() {
        let (_tmp, s, sid) = fixture();
        s.ensure_session_dir(&sid, false).unwrap();
        assert!(s.read_session_meta(&sid).unwrap().is_none());
    }

    #[test]
    fn list_sessions_finds_all_active() {
        let (_tmp, s, _sid) = fixture();
        for id in ["sess_a", "sess_b", "sess_c"] {
            let sid = SessionId::parse(id).unwrap();
            s.ensure_session_dir(&sid, false).unwrap();
        }
        let mut listed: Vec<String> = s
            .list_sessions()
            .unwrap()
            .into_iter()
            .map(SessionId::into_inner)
            .collect();
        listed.sort();
        assert_eq!(listed, vec!["sess_a", "sess_b", "sess_c"]);
    }

    #[test]
    fn remove_session_clears_dir() {
        let (_tmp, s, sid) = fixture();
        s.write_turn(&sid, false, 0, &turn(0, "hi")).unwrap();
        s.remove_session(&sid).unwrap();
        assert!(!s.session_dir(&sid, false).exists());
    }

    #[test]
    fn append_log_creates_log_file() {
        let (_tmp, s, _sid) = fixture();
        s.append_log("hook user-prompt-submit: ok").unwrap();
        s.append_log("hook post-commit: 0 matched").unwrap();
        let body = fs::read_to_string(s.root().join(LOG_FILENAME)).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
    }
}
