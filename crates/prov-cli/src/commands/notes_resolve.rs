//! `prov notes-resolve` — finish a `git notes merge` that conflicted.
//!
//! `prov install` sets `notes.mergeStrategy=manual`, so when `prov fetch` brings
//! in a remote notes ref that diverged from local, `git notes --ref=prompts
//! merge` parks each conflicting commit's note in
//! `.git/NOTES_MERGE_WORKTREE/<commit-sha>` with `<<<<<<<` / `=======` /
//! `>>>>>>>` markers. `git notes merge --commit` refuses to finalize until
//! every file there parses as a clean note.
//!
//! This command does the JSON-aware merge: parse both sides as `Note`, union
//! their `edits[]`, dedupe by `(conversation_id, turn_index, tool_use_id)`
//! (falling back to a content-hash digest when `tool_use_id` is `None` on both
//! sides), keep the entry with the later `timestamp` on collision, sort the
//! result by `timestamp`, write it back, and finalize. Two devs annotating
//! different turns produce a merged note containing both turns; the impossible
//! case where both annotated the same `tool_use_id` keeps the later one rather
//! than dropping data silently.
//!
//! Schema-version mismatch on either side aborts before any write so a v1
//! reader does not fabricate a v2-shaped union.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use clap::Parser;

use prov_core::git::{Git, GitError};
use prov_core::schema::{Edit, Note, SchemaError, SCHEMA_VERSION};
use prov_core::storage::NOTES_REF_PUBLIC;

use super::common::invalidate_cache_per_sha;

#[derive(Parser, Debug)]
pub struct Args {}

#[allow(clippy::needless_pass_by_value)]
pub fn run(_args: Args) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = Git::discover(&cwd).map_err(|e| match e {
        GitError::NotARepo => anyhow!("not in a git repo"),
        other => anyhow::Error::from(other),
    })?;

    let merge_ref_path = git.git_dir().join("NOTES_MERGE_REF");
    let worktree_path = git.git_dir().join("NOTES_MERGE_WORKTREE");
    if !merge_ref_path.exists() {
        println!("prov notes-resolve: no merge to resolve");
        return Ok(());
    }

    let conflict_files = collect_conflict_files(&worktree_path)
        .with_context(|| format!("walking {}", worktree_path.display()))?;

    let mut resolved_shas: Vec<String> = Vec::new();
    for path in &conflict_files {
        let body =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let sha = sha_from_worktree_path(&worktree_path, path)
            .ok_or_else(|| anyhow!("could not derive commit SHA from {}", path.display()))?;

        let merged_json = if let Some((local_json, incoming_json)) = split_conflict(&body) {
            merge_note_pair(&sha, &local_json, &incoming_json)?
        } else {
            // No conflict markers: the file is either already a clean
            // resolution the user wrote, or git auto-merged this slot but
            // left it for us to round-trip. Validate it parses as a v1 note
            // so `--commit` won't choke on it.
            Note::from_json(&body)
                .with_context(|| format!("{} is not a valid prov note", path.display()))?;
            body.trim().to_string()
        };
        fs::write(path, merged_json.as_bytes())
            .with_context(|| format!("writing merged note to {}", path.display()))?;
        resolved_shas.push(sha);
    }

    // `git notes merge --commit` infers the target ref from `NOTES_MERGE_REF`
    // — passing `--ref` here would force a specific ref and silently break
    // resolution for any future ref a user might be merging (e.g. someone
    // manually merging into `refs/notes/prompts-private`).
    git.run(["notes", "merge", "--commit"])
        .context("git notes merge --commit failed")?;

    invalidate_cache_per_sha(&git, resolved_shas.iter().map(String::as_str));

    let count = conflict_files.len();
    let target = read_merge_target_ref(&merge_ref_path).unwrap_or_else(|| NOTES_REF_PUBLIC.into());
    if count == 0 {
        println!("prov notes-resolve: finalized {target} (no conflicts to merge)");
    } else {
        println!("prov notes-resolve: finalized {target} ({count} conflict(s) merged)");
    }
    Ok(())
}

/// Walk `NOTES_MERGE_WORKTREE` and return every regular file underneath it.
/// Older git releases (and the typical fan-out for large notes refs) split
/// the commit SHA across one or two directory levels; the recursive walk
/// handles both flat and split layouts.
fn collect_conflict_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Reconstruct the annotated commit SHA from a worktree path. The file's
/// path components below `root` are concatenated and stripped of separators —
/// flat (`<sha>`) and split (`<aa>/<rest>`) layouts both yield the 40-char
/// hex SHA.
fn sha_from_worktree_path(root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?;
    let mut joined = String::new();
    for comp in rel.components() {
        if let std::path::Component::Normal(s) = comp {
            joined.push_str(s.to_str()?);
        }
    }
    if joined.len() == 40 && joined.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(joined)
    } else {
        None
    }
}

/// Split a conflict-marked file into the local and incoming JSON bodies.
/// Returns `None` when the file lacks markers (already resolved).
fn split_conflict(body: &str) -> Option<(String, String)> {
    let mut local = String::new();
    let mut incoming = String::new();
    let mut state = ConflictState::Outside;
    let mut saw_marker = false;
    for line in body.lines() {
        if line.starts_with("<<<<<<<") {
            state = ConflictState::Local;
            saw_marker = true;
        } else if line.starts_with("=======") && matches!(state, ConflictState::Local) {
            state = ConflictState::Incoming;
        } else if line.starts_with(">>>>>>>") && matches!(state, ConflictState::Incoming) {
            state = ConflictState::Outside;
        } else {
            match state {
                ConflictState::Local => {
                    local.push_str(line);
                    local.push('\n');
                }
                ConflictState::Incoming => {
                    incoming.push_str(line);
                    incoming.push('\n');
                }
                ConflictState::Outside => {
                    // Content outside the conflict block belongs to both sides
                    // (git's manual strategy doesn't normally emit any, but be
                    // defensive — append to both so neither loses context).
                    local.push_str(line);
                    local.push('\n');
                    incoming.push_str(line);
                    incoming.push('\n');
                }
            }
        }
    }
    if saw_marker {
        Some((local, incoming))
    } else {
        None
    }
}

enum ConflictState {
    Outside,
    Local,
    Incoming,
}

/// Merge two note JSON bodies. Schema-version mismatch on either side aborts
/// rather than fabricating a union shaped after one side's schema.
fn merge_note_pair(sha: &str, local_json: &str, incoming_json: &str) -> anyhow::Result<String> {
    let local = parse_note_with_context(sha, "local", local_json)?;
    let incoming = parse_note_with_context(sha, "incoming", incoming_json)?;

    if local.version != incoming.version {
        return Err(anyhow!(
            "schema version mismatch on {sha}: local v{local_v} vs incoming v{incoming_v} \
             (this build supports v{SCHEMA_VERSION}); aborting merge to avoid corrupting either side. \
             Run `git notes --ref=prompts merge --abort` and upgrade prov before retrying.",
            local_v = local.version,
            incoming_v = incoming.version,
        ));
    }

    let mut merged: BTreeMap<EditKey, Edit> = BTreeMap::new();
    for edit in local.edits.into_iter().chain(incoming.edits.into_iter()) {
        let key = EditKey::from_edit(&edit);
        match merged.get(&key) {
            Some(existing) if existing.timestamp >= edit.timestamp => {}
            _ => {
                merged.insert(key, edit);
            }
        }
    }

    let mut edits: Vec<Edit> = merged.into_values().collect();
    // Stable sort by (timestamp, conversation_id, turn_index) so two edits
    // sharing a timestamp don't reorder run-to-run on the same input.
    edits.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.conversation_id.cmp(&b.conversation_id))
            .then_with(|| a.turn_index.cmp(&b.turn_index))
    });

    let merged_note = Note::new(edits);
    merged_note
        .to_json()
        .map_err(|e| anyhow!("serializing merged note for {sha}: {e}"))
}

fn parse_note_with_context(sha: &str, side: &str, json: &str) -> anyhow::Result<Note> {
    Note::from_json(json).map_err(|e| match e {
        SchemaError::UnknownVersion(v) => anyhow!(
            "{side} note for {sha} has schema version v{v}; this build of prov supports \
             v{SCHEMA_VERSION}. Aborting merge to avoid data loss; upgrade prov on this machine \
             (or `git notes --ref=prompts merge --abort` to back out)."
        ),
        other => anyhow!("{side} note for {sha} did not parse: {other}"),
    })
}

/// Dedup key for `edits[]`. `tool_use_id` is the strongest signal, but capture
/// from older Claude Code builds may leave it `None`; in that case the joined
/// `content_hashes` discriminate edits within the same turn so a shared
/// `(conversation_id, turn_index)` doesn't silently collapse two unrelated
/// edits into one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EditKey {
    conversation_id: String,
    turn_index: u32,
    discriminator: String,
}

impl EditKey {
    fn from_edit(edit: &Edit) -> Self {
        let discriminator = edit.tool_use_id.clone().unwrap_or_else(|| {
            // Fallback: join hashes with `|` (not a hex char, so no aliasing).
            edit.content_hashes.join("|")
        });
        Self {
            conversation_id: edit.conversation_id.clone(),
            turn_index: edit.turn_index,
            discriminator,
        }
    }
}

fn read_merge_target_ref(merge_ref_path: &Path) -> Option<String> {
    let raw = fs::read_to_string(merge_ref_path).ok()?;
    let trimmed = raw.trim();
    Some(trimmed.strip_prefix("ref: ").unwrap_or(trimmed).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prov_core::schema::{Edit, Note};

    fn edit(conv: &str, turn: u32, tool: Option<&str>, ts: &str, hash: &str) -> Edit {
        Edit {
            file: "x.rs".into(),
            line_range: [1, 1],
            content_hashes: vec![hash.into()],
            original_blob_sha: None,
            prompt: format!("p-{conv}-{turn}"),
            conversation_id: conv.into(),
            turn_index: turn,
            tool_use_id: tool.map(String::from),
            preceding_turns_summary: None,
            model: "m".into(),
            tool: "claude-code".into(),
            timestamp: ts.into(),
            derived_from: None,
        }
    }

    fn conflict_body(local: &Note, incoming: &Note) -> String {
        format!(
            "<<<<<<< refs/notes/prompts\n{}\n=======\n{}\n>>>>>>> refs/notes/origin/prompts\n",
            local.to_json().unwrap(),
            incoming.to_json().unwrap(),
        )
    }

    #[test]
    fn split_conflict_extracts_both_sides() {
        let local = Note::new(vec![edit("a", 0, None, "2026-01-01T00:00:00Z", "h1")]);
        let incoming = Note::new(vec![edit("b", 1, None, "2026-01-02T00:00:00Z", "h2")]);
        let body = conflict_body(&local, &incoming);
        let (l, i) = split_conflict(&body).expect("markers present");
        assert!(l.contains("\"conversation_id\": \"a\""));
        assert!(i.contains("\"conversation_id\": \"b\""));
    }

    #[test]
    fn split_conflict_returns_none_for_already_resolved() {
        let n = Note::new(vec![edit("a", 0, None, "2026-01-01T00:00:00Z", "h1")]);
        let json = n.to_json().unwrap();
        assert!(split_conflict(&json).is_none());
    }

    #[test]
    fn merge_unions_disjoint_edits() {
        let local = Note::new(vec![edit("a", 0, Some("t1"), "2026-01-01T00:00:00Z", "h1")]);
        let incoming = Note::new(vec![edit("b", 5, Some("t2"), "2026-01-02T00:00:00Z", "h2")]);
        let local_json = local.to_json().unwrap();
        let incoming_json = incoming.to_json().unwrap();
        let merged = merge_note_pair("deadbeef", &local_json, &incoming_json).unwrap();
        let parsed = Note::from_json(&merged).unwrap();
        assert_eq!(parsed.edits.len(), 2);
        assert_eq!(parsed.edits[0].conversation_id, "a");
        assert_eq!(parsed.edits[1].conversation_id, "b");
    }

    #[test]
    fn merge_dedupes_by_tool_use_id_keeping_later_timestamp() {
        let local = Note::new(vec![edit(
            "a",
            0,
            Some("t-shared"),
            "2026-01-01T00:00:00Z",
            "h1",
        )]);
        let incoming = Note::new(vec![edit(
            "a",
            0,
            Some("t-shared"),
            "2026-01-02T00:00:00Z",
            "h2",
        )]);
        let merged = merge_note_pair(
            "deadbeef",
            &local.to_json().unwrap(),
            &incoming.to_json().unwrap(),
        )
        .unwrap();
        let parsed = Note::from_json(&merged).unwrap();
        assert_eq!(parsed.edits.len(), 1);
        assert_eq!(parsed.edits[0].timestamp, "2026-01-02T00:00:00Z");
        // Later side wins, so the later edit's content_hashes should survive.
        assert_eq!(parsed.edits[0].content_hashes, vec!["h2".to_string()]);
    }

    #[test]
    fn merge_falls_back_to_content_hash_when_tool_use_id_is_none() {
        // Same conversation+turn+None tool_use_id but different content
        // hashes — these are distinct edits captured by an older client and
        // should not collapse.
        let local = Note::new(vec![edit("a", 0, None, "2026-01-01T00:00:00Z", "h1")]);
        let incoming = Note::new(vec![edit("a", 0, None, "2026-01-02T00:00:00Z", "h2")]);
        let merged = merge_note_pair(
            "deadbeef",
            &local.to_json().unwrap(),
            &incoming.to_json().unwrap(),
        )
        .unwrap();
        let parsed = Note::from_json(&merged).unwrap();
        assert_eq!(parsed.edits.len(), 2);
    }

    #[test]
    fn merge_sorts_by_timestamp() {
        let local = Note::new(vec![
            edit("a", 1, Some("t2"), "2026-01-02T00:00:00Z", "h2"),
            edit("a", 0, Some("t1"), "2026-01-01T00:00:00Z", "h1"),
        ]);
        let incoming = Note::new(vec![edit("a", 2, Some("t3"), "2026-01-03T00:00:00Z", "h3")]);
        let merged = merge_note_pair(
            "deadbeef",
            &local.to_json().unwrap(),
            &incoming.to_json().unwrap(),
        )
        .unwrap();
        let parsed = Note::from_json(&merged).unwrap();
        let timestamps: Vec<&str> = parsed.edits.iter().map(|e| e.timestamp.as_str()).collect();
        assert_eq!(
            timestamps,
            vec![
                "2026-01-01T00:00:00Z",
                "2026-01-02T00:00:00Z",
                "2026-01-03T00:00:00Z",
            ]
        );
    }

    #[test]
    fn merge_rejects_unknown_schema_version() {
        let bad = r#"{"version":99,"edits":[]}"#;
        let good = Note::new(vec![]).to_json().unwrap();
        let err = merge_note_pair("deadbeef", bad, &good).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("schema version") && msg.contains("v99"),
            "expected schema-version error mentioning v99, got: {msg}"
        );
    }

    #[test]
    fn sha_from_flat_layout() {
        let root = Path::new("/tmp/x/NOTES_MERGE_WORKTREE");
        let file =
            Path::new("/tmp/x/NOTES_MERGE_WORKTREE/abcdef0123456789abcdef0123456789abcdef01");
        assert_eq!(
            sha_from_worktree_path(root, file).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
    }

    #[test]
    fn sha_from_split_layout() {
        let root = Path::new("/tmp/x/NOTES_MERGE_WORKTREE");
        let file =
            Path::new("/tmp/x/NOTES_MERGE_WORKTREE/ab/cdef0123456789abcdef0123456789abcdef01");
        assert_eq!(
            sha_from_worktree_path(root, file).as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef01")
        );
    }

    #[test]
    fn sha_rejects_non_hex_path() {
        let root = Path::new("/tmp/x/NOTES_MERGE_WORKTREE");
        let file = Path::new("/tmp/x/NOTES_MERGE_WORKTREE/not-a-sha");
        assert!(sha_from_worktree_path(root, file).is_none());
    }

    #[test]
    fn read_merge_target_ref_strips_symref_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("NOTES_MERGE_REF");
        fs::write(&p, "ref: refs/notes/prompts\n").unwrap();
        assert_eq!(
            read_merge_target_ref(&p).as_deref(),
            Some("refs/notes/prompts")
        );
    }
}
