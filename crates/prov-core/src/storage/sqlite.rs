//! SQLite-backed cache derived from the notes ref.
//!
//! Enables sub-50ms `prov log` and `prov search` lookups without re-shelling
//! `git notes show` per query. Stores three tables plus an FTS5 virtual table
//! for full-text search over prompt bodies, and a `cache_meta` row tracking
//! the notes-ref SHA at last reindex so reads can detect drift after `prov
//! fetch` or external `git notes` writes.

use crate::schema::{Note, SchemaError};
use crate::storage::notes::{NotesError, NotesStore};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// SQLite cache schema version for prov's local cache (independent of the note
/// JSON schema version). Bump only when prov's cache table layout changes.
pub const CACHE_SCHEMA_VERSION: u32 = 1;

const CACHE_META_NOTES_REF_SHA: &str = "notes_ref_sha";

/// Cache rooted at a single SQLite file (typically `<git_dir>/prov.db`).
pub struct Cache {
    conn: Connection,
}

impl Cache {
    /// Open or create the cache file at the given path. Initializes the schema
    /// on first use; subsequent opens are cheap.
    ///
    /// Refuses to open caches stamped with a `cache_schema_version` other than
    /// the version this build supports. Callers (`prov reindex`) handle the
    /// `SchemaVersionMismatch` variant by suggesting a rebuild.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, CacheError> {
        let conn = Connection::open(path).map_err(CacheError::from)?;
        Self::init(&conn)?;
        check_schema_version(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory cache. Used by tests.
    pub fn open_in_memory() -> Result<Self, CacheError> {
        let conn = Connection::open_in_memory().map_err(CacheError::from)?;
        Self::init(&conn)?;
        check_schema_version(&conn)?;
        Ok(Self { conn })
    }

    fn init(conn: &Connection) -> Result<(), CacheError> {
        conn.execute_batch(
            r"
            PRAGMA journal_mode = WAL;
            PRAGMA busy_timeout = 5000;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS cache_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS notes (
                commit_sha TEXT PRIMARY KEY,
                json       TEXT NOT NULL,
                fetched_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS edits (
                commit_sha       TEXT NOT NULL,
                edit_idx         INTEGER NOT NULL,
                file             TEXT NOT NULL,
                line_start       INTEGER NOT NULL,
                line_end         INTEGER NOT NULL,
                prompt           TEXT NOT NULL,
                conversation_id  TEXT NOT NULL,
                model            TEXT NOT NULL,
                ts               TEXT NOT NULL,
                PRIMARY KEY (commit_sha, edit_idx),
                FOREIGN KEY (commit_sha) REFERENCES notes(commit_sha) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS edits_by_file ON edits(file);
            CREATE INDEX IF NOT EXISTS edits_by_file_range ON edits(file, line_start, line_end);

            CREATE TABLE IF NOT EXISTS content_hashes (
                commit_sha TEXT NOT NULL,
                edit_idx   INTEGER NOT NULL,
                line_idx   INTEGER NOT NULL,
                hash       TEXT NOT NULL,
                PRIMARY KEY (commit_sha, edit_idx, line_idx),
                FOREIGN KEY (commit_sha, edit_idx)
                    REFERENCES edits(commit_sha, edit_idx) ON DELETE CASCADE
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS edits_fts USING fts5(
                prompt,
                commit_sha UNINDEXED,
                edit_idx UNINDEXED,
                content='edits',
                content_rowid='rowid'
            );
            ",
        )?;
        // Stamp the cache schema version so future cache migrations can detect drift.
        conn.execute(
            "INSERT OR IGNORE INTO cache_meta(key, value) VALUES (?1, ?2)",
            params!["cache_schema_version", CACHE_SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    /// Notes-ref SHA recorded at last reindex, or `None` if none recorded yet.
    pub fn recorded_notes_ref_sha(&self) -> Result<Option<String>, CacheError> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key = ?1",
                params![CACHE_META_NOTES_REF_SHA],
                |row| row.get(0),
            )
            .optional()?;
        Ok(v)
    }

    #[cfg(test)]
    fn set_recorded_notes_ref_sha(&self, sha: Option<&str>) -> Result<(), CacheError> {
        match sha {
            Some(s) => {
                self.conn.execute(
                    "INSERT INTO cache_meta(key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![CACHE_META_NOTES_REF_SHA, s],
                )?;
            }
            None => {
                self.conn.execute(
                    "DELETE FROM cache_meta WHERE key = ?1",
                    params![CACHE_META_NOTES_REF_SHA],
                )?;
            }
        }
        Ok(())
    }

    /// Drop and rebuild every table from the given `NotesStore`. After this,
    /// `recorded_notes_ref_sha` matches the store's `ref_sha` so subsequent
    /// reads can detect drift.
    ///
    /// All writes — including the `cache_meta.notes_ref_sha` stamp — commit
    /// inside a single transaction. If anything fails the cache is rolled back
    /// to its prior state rather than left half-rebuilt with a stale SHA.
    pub fn reindex_from(&mut self, store: &NotesStore) -> Result<ReindexStats, CacheError> {
        // Capture the live ref SHA before opening the transaction so the SHA
        // stamp inside the tx reflects the state we actually rebuilt from.
        let ref_sha = store.ref_sha()?;
        let entries = store.list().map_err(CacheError::from)?;

        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM notes", [])?;
        // edits and content_hashes cascade via FK from notes deletion.
        // External-content FTS5 tables don't auto-clear when the content table is deleted —
        // use the FTS5 `delete-all` command, then re-populate from new edits below.
        tx.execute("INSERT INTO edits_fts(edits_fts) VALUES('delete-all')", [])?;

        let mut note_count = 0_u32;
        let mut edit_count = 0_u32;
        let now = unix_now();

        for (sha, note) in &entries {
            tx.execute(
                "INSERT INTO notes(commit_sha, json, fetched_at) VALUES (?1, ?2, ?3)",
                params![sha, note.to_json()?, now],
            )?;
            note_count += 1;

            for (idx, edit) in note.edits.iter().enumerate() {
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let edit_idx_i64 = idx as i64;
                tx.execute(
                    "INSERT INTO edits(commit_sha, edit_idx, file, line_start, line_end,
                                       prompt, conversation_id, model, ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        sha,
                        edit_idx_i64,
                        edit.file,
                        i64::from(edit.line_range[0]),
                        i64::from(edit.line_range[1]),
                        edit.prompt,
                        edit.conversation_id,
                        edit.model,
                        edit.timestamp,
                    ],
                )?;
                edit_count += 1;

                tx.execute(
                    "INSERT INTO edits_fts(rowid, prompt, commit_sha, edit_idx)
                     SELECT rowid, prompt, ?1, ?2 FROM edits
                     WHERE commit_sha = ?1 AND edit_idx = ?2",
                    params![sha, edit_idx_i64],
                )?;

                for (line_idx, hash) in edit.content_hashes.iter().enumerate() {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    let line_idx_i64 = line_idx as i64;
                    tx.execute(
                        "INSERT INTO content_hashes(commit_sha, edit_idx, line_idx, hash)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![sha, edit_idx_i64, line_idx_i64, hash],
                    )?;
                }
            }
        }

        // Stamp the SHA in the same transaction so a failure rolls back the
        // entire reindex (rather than leaving rebuilt rows under a stale SHA).
        write_recorded_notes_ref_sha_tx(&tx, ref_sha.as_deref())?;
        tx.commit()?;

        Ok(ReindexStats {
            notes: note_count,
            edits: edit_count,
        })
    }

    /// Look up the cached note for `commit_sha`. Returns `Ok(None)` if absent.
    pub fn get_note(&self, commit_sha: &str) -> Result<Option<Note>, CacheError> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM notes WHERE commit_sha = ?1",
                params![commit_sha],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(j) => Ok(Some(Note::from_json(&j)?)),
            None => Ok(None),
        }
    }

    /// Find every cached edit touching `file`, ordered by commit timestamp.
    pub fn edits_for_file(&self, file: &str) -> Result<Vec<EditRow>, CacheError> {
        let mut stmt = self.conn.prepare(
            "SELECT commit_sha, edit_idx, file, line_start, line_end,
                    prompt, conversation_id, model, ts
             FROM edits
             WHERE file = ?1
             ORDER BY ts DESC",
        )?;
        let rows = stmt
            .query_map(params![file], EditRow::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// FTS5 search over prompt bodies. Returns matches ordered by recency.
    /// Caller is responsible for escaping FTS5 syntax if exposing to untrusted input.
    pub fn search_prompts(&self, query: &str, limit: u32) -> Result<Vec<EditRow>, CacheError> {
        let mut stmt = self.conn.prepare(
            "SELECT e.commit_sha, e.edit_idx, e.file, e.line_start, e.line_end,
                    e.prompt, e.conversation_id, e.model, e.ts
             FROM edits_fts f
             JOIN edits e ON e.commit_sha = f.commit_sha AND e.edit_idx = f.edit_idx
             WHERE edits_fts MATCH ?1
             ORDER BY e.ts DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![query, i64::from(limit)], EditRow::from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Total notes currently cached.
    pub fn note_count(&self) -> Result<u32, CacheError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))?;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(n as u32)
    }
}

/// Stats returned by `reindex_from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReindexStats {
    /// Number of notes copied from the notes ref.
    pub notes: u32,
    /// Total edits across all copied notes.
    pub edits: u32,
}

/// One row from the `edits` table (also returned by FTS searches).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRow {
    /// Commit SHA the edit is attributed to.
    pub commit_sha: String,
    /// Index into the note's `edits[]` array.
    pub edit_idx: u32,
    /// File path.
    pub file: String,
    /// Inclusive line range start.
    pub line_start: u32,
    /// Inclusive line range end.
    pub line_end: u32,
    /// Originating prompt (post-redaction).
    pub prompt: String,
    /// Claude Code session id.
    pub conversation_id: String,
    /// Model name captured at session start.
    pub model: String,
    /// ISO-8601 timestamp.
    pub timestamp: String,
}

impl EditRow {
    fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            commit_sha: row.get(0)?,
            edit_idx: u32_from_i64(row.get(1)?),
            file: row.get(2)?,
            line_start: u32_from_i64(row.get(3)?),
            line_end: u32_from_i64(row.get(4)?),
            prompt: row.get(5)?,
            conversation_id: row.get(6)?,
            model: row.get(7)?,
            timestamp: row.get(8)?,
        })
    }
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn u32_from_i64(v: i64) -> u32 {
    v as u32
}

/// Read the stored `cache_schema_version` and reject if it does not match this
/// build's `CACHE_SCHEMA_VERSION`. A missing row (fresh DB; `init` only stamps
/// it via `INSERT OR IGNORE`) is treated as the current version.
fn check_schema_version(conn: &Connection) -> Result<(), CacheError> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM cache_meta WHERE key = 'cache_schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(());
    };
    let stored_n: u32 = stored
        .parse()
        .map_err(|_| CacheError::SchemaVersionMismatch {
            stored: 0,
            expected: CACHE_SCHEMA_VERSION,
        })?;
    if stored_n == CACHE_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(CacheError::SchemaVersionMismatch {
            stored: stored_n,
            expected: CACHE_SCHEMA_VERSION,
        })
    }
}

/// Upsert (or delete) `cache_meta.notes_ref_sha`, bound to an open
/// `Transaction` so the SHA stamp commits or rolls back atomically with the
/// reindex writes.
fn write_recorded_notes_ref_sha_tx(
    tx: &rusqlite::Transaction<'_>,
    sha: Option<&str>,
) -> Result<(), CacheError> {
    match sha {
        Some(s) => {
            tx.execute(
                "INSERT INTO cache_meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![CACHE_META_NOTES_REF_SHA, s],
            )?;
        }
        None => {
            tx.execute(
                "DELETE FROM cache_meta WHERE key = ?1",
                params![CACHE_META_NOTES_REF_SHA],
            )?;
        }
    }
    Ok(())
}

fn unix_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Errors raised by `Cache`.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// Underlying SQLite error.
    #[error("sqlite error: {0}")]
    Sqlite(String),
    /// Underlying notes-store error.
    #[error(transparent)]
    Notes(#[from] NotesError),
    /// Note schema parsing error.
    #[error(transparent)]
    Schema(#[from] SchemaError),
    /// Stored cache schema version does not match this build of prov. Callers
    /// should drop the cache file and run `prov reindex` to rebuild.
    #[error("cache schema version mismatch: stored v{stored}, this build of prov supports v{expected}; run `prov reindex` to rebuild")]
    SchemaVersionMismatch {
        /// Version recorded in `cache_meta.cache_schema_version`.
        stored: u32,
        /// Version the running build expects.
        expected: u32,
    },
}

impl From<rusqlite::Error> for CacheError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_initializes_schema() {
        let cache = Cache::open_in_memory().unwrap();
        // cache_schema_version row should exist.
        let v: String = cache
            .conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key = 'cache_schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, CACHE_SCHEMA_VERSION.to_string());
        assert_eq!(cache.note_count().unwrap(), 0);
    }

    #[test]
    fn recorded_notes_ref_sha_starts_none() {
        let cache = Cache::open_in_memory().unwrap();
        assert!(cache.recorded_notes_ref_sha().unwrap().is_none());
    }

    #[test]
    fn set_and_get_recorded_notes_ref_sha() {
        let cache = Cache::open_in_memory().unwrap();
        cache.set_recorded_notes_ref_sha(Some("abc")).unwrap();
        assert_eq!(
            cache.recorded_notes_ref_sha().unwrap().as_deref(),
            Some("abc")
        );
        cache.set_recorded_notes_ref_sha(None).unwrap();
        assert!(cache.recorded_notes_ref_sha().unwrap().is_none());
    }

    // Reindex + search tests live in the crate-level integration tests under
    // `crates/prov-core/tests/storage.rs` so they can spin up a real fixture
    // git repo + NotesStore.
}
