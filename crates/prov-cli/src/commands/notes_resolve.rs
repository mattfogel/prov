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
//! (falling back to `(file, line_range)` when `tool_use_id` is `None` on both
//! sides — same shape as `hook.rs::dedupe_and_sort_edits` so the two dedupers
//! stay in sync), keep the entry with the later `timestamp` on collision, sort
//! the result by `timestamp`, write it back, and finalize. Two devs annotating
//! different turns produce a merged note containing both turns; the impossible
//! case where both annotated the same `tool_use_id` keeps the later one rather
//! than dropping data silently.
//!
//! Schema-version mismatch on either side aborts the current file before any
//! write so a v1 reader does not fabricate a v2-shaped union. Across multiple
//! conflict files, an abort on file N leaves files 1..N-1 already rewritten
//! on disk; rerunning `prov notes-resolve` revalidates and finalizes correctly
//! because resolved files round-trip cleanly through the no-marker branch.

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

    // Resolve the actual ref being merged so user-facing error messages can
    // point at the right `git notes --ref=<X> merge --abort` to back out. The
    // success-path `--commit` below deliberately omits `--ref` so the resolver
    // works for any future ref a user might be merging (e.g. someone manually
    // merging into `refs/notes/prompts-private`); the abort hint should track
    // that same ref rather than hardcoding `prompts`.
    let target_ref =
        read_merge_target_ref(&merge_ref_path).unwrap_or_else(|| NOTES_REF_PUBLIC.into());

    let mut resolved_shas: Vec<String> = Vec::new();
    for path in &conflict_files {
        let body =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let sha = sha_from_worktree_path(&worktree_path, path)
            .ok_or_else(|| anyhow!("could not derive commit SHA from {}", path.display()))?;

        let merged_json = if let Some((local_json, incoming_json)) = split_conflict(&body) {
            merge_note_pair(&sha, &target_ref, &local_json, &incoming_json)?
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

    let count = conflict_files.len();
    // Surface the silent-finalize edge case before deferring to git: if
    // NOTES_MERGE_REF is set but no conflict files were found in
    // NOTES_MERGE_WORKTREE (resolver killed mid-loop, partial cleanup, etc.),
    // git's behavior with `--commit` is version-dependent. Warn the user so
    // an inconsistent state isn't masked by a bland success message.
    if count == 0 {
        eprintln!(
            "warning: no conflict files found in NOTES_MERGE_WORKTREE; \
             merge may be in inconsistent state"
        );
    }

    // `git notes merge --commit` infers the target ref from `NOTES_MERGE_REF`
    // — passing `--ref` here would force a specific ref and silently break
    // resolution for any future ref a user might be merging (e.g. someone
    // manually merging into `refs/notes/prompts-private`).
    git.run(["notes", "merge", "--commit"])
        .context("git notes merge --commit failed")?;

    invalidate_cache_per_sha(&git, resolved_shas.iter().map(String::as_str));

    if count == 0 {
        println!("prov notes-resolve: finalized {target_ref} (no conflicts to merge)");
    } else {
        println!("prov notes-resolve: finalized {target_ref} ({count} conflict(s) merged)");
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
///
/// Returns `Some((local, incoming))` when conflict markers were present, or
/// `None` when the file lacks markers entirely (already resolved by git or
/// the user; the caller validates it parses as a v1 note).
///
/// `git notes merge`'s textual diff leaves matching prefix and suffix lines
/// *outside* the conflict markers when the JSON is pretty-printed (e.g. the
/// shared `{`, `"version": 1,`, `}`); those lines genuinely belong to both
/// sides, so this function appends them to both buffers. That's intentional,
/// not defensive — without it, neither side would parse as valid JSON.
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
                    // git's diff3-style merge places shared prefix/suffix
                    // (matching JSON braces, version field, etc.) OUTSIDE the
                    // markers — those lines genuinely belong to both sides.
                    // Appending them to both buffers reconstructs each side's
                    // original full body so the JSON parser sees a valid note.
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
/// rather than fabricating a union shaped after one side's schema. `target_ref`
/// is the notes ref currently being merged (read from `NOTES_MERGE_REF`); it
/// is woven into user-facing error messages so the suggested
/// `git notes --ref=<X> merge --abort` matches the actual ref in flight rather
/// than hardcoding `prompts`.
fn merge_note_pair(
    sha: &str,
    target_ref: &str,
    local_json: &str,
    incoming_json: &str,
) -> anyhow::Result<String> {
    let local = parse_note_with_context(sha, target_ref, "local", local_json)?;
    let incoming = parse_note_with_context(sha, target_ref, "incoming", incoming_json)?;

    // Defensive: dead today since `Note::from_json` rejects mismatched
    // versions before we get here. Kept for future schema-range support
    // (e.g. accepting v1+v2 reads through the same parser).
    if local.version != incoming.version {
        return Err(anyhow!(
            "schema version mismatch on {sha}: local v{local_v} vs incoming v{incoming_v} \
             (this build supports v{SCHEMA_VERSION}); aborting merge to avoid corrupting either side. \
             Run `git notes --ref={target_ref} merge --abort` and upgrade prov before retrying.",
            local_v = local.version,
            incoming_v = incoming.version,
        ));
    }

    let mut merged: BTreeMap<EditKey, Edit> = BTreeMap::new();
    for edit in local.edits.into_iter().chain(incoming.edits) {
        let key = EditKey::from_edit(&edit);
        match merged.get(&key) {
            Some(existing) if existing.timestamp >= edit.timestamp => {}
            _ => {
                merged.insert(key, edit);
            }
        }
    }

    let mut edits: Vec<Edit> = merged.into_values().collect();
    // Stable sort by (timestamp, conversation_id, turn_index, tool_use_id) so
    // two edits sharing the leading keys don't reorder run-to-run on the same
    // input. The `tool_use_id` tiebreaker locks determinism in the sort itself
    // rather than relying on the upstream BTreeMap's iteration order — a
    // future swap to HashMap (or any other unordered collection) would
    // otherwise silently break ordering.
    edits.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.conversation_id.cmp(&b.conversation_id))
            .then_with(|| a.turn_index.cmp(&b.turn_index))
            .then_with(|| a.tool_use_id.cmp(&b.tool_use_id))
    });

    let merged_note = Note::new(edits);
    merged_note
        .to_json()
        .map_err(|e| anyhow!("serializing merged note for {sha}: {e}"))
}

fn parse_note_with_context(
    sha: &str,
    target_ref: &str,
    side: &str,
    json: &str,
) -> anyhow::Result<Note> {
    Note::from_json(json).map_err(|e| match e {
        SchemaError::UnknownVersion(v) => anyhow!(
            "{side} note for {sha} has schema version v{v}; this build of prov supports \
             v{SCHEMA_VERSION}. Aborting merge to avoid data loss; upgrade prov on this machine \
             (or `git notes --ref={target_ref} merge --abort` to back out)."
        ),
        SchemaError::MissingVersion => anyhow!(
            "{side} note for {sha} is missing a 'version' field — likely truncated \
             or malformed. Inspect with `git notes show {sha}` and abort the merge \
             if needed (`git notes --ref={target_ref} merge --abort`)."
        ),
        other => anyhow!("{side} note for {sha} did not parse: {other}"),
    })
}

/// Dedup key for `edits[]`. `tool_use_id` is the strongest signal, but
/// capture from older Claude Code builds (or `prov backfill`-style synthesized
/// edits) may leave it `None`; the fallback discriminator distinguishes
/// distinct edits that share `(conversation_id, turn_index)`.
///
/// Shape mirrors `hook.rs::dedupe_and_sort_edits` so the two dedupers stay in
/// sync — `tool_use_id` is held in its own slot rather than collapsed into a
/// generic string, and the fallback uses `(file, line_range)` which is
/// stable across re-captures and can't collide with a `tool_use_id` value.
/// Holding `tool_use_id` in a dedicated slot prevents an exotic id containing
/// the fallback's `@` / `-` separators from masquerading as another edit's
/// fallback discriminator.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EditKey {
    conversation_id: String,
    turn_index: u32,
    tool_use_id: Option<String>,
    /// Empty when `tool_use_id` is `Some(_)`; otherwise `"{file}@{start}-{end}"`.
    fallback: String,
}

impl EditKey {
    fn from_edit(edit: &Edit) -> Self {
        let fallback = if edit.tool_use_id.is_none() {
            format!(
                "{}@{}-{}",
                edit.file, edit.line_range[0], edit.line_range[1]
            )
        } else {
            String::new()
        };
        Self {
            conversation_id: edit.conversation_id.clone(),
            turn_index: edit.turn_index,
            tool_use_id: edit.tool_use_id.clone(),
            fallback,
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
        edit_at(conv, turn, tool, ts, hash, "x.rs", [1, 1])
    }

    fn edit_at(
        conv: &str,
        turn: u32,
        tool: Option<&str>,
        ts: &str,
        hash: &str,
        file: &str,
        line_range: [u32; 2],
    ) -> Edit {
        Edit {
            file: file.into(),
            line_range,
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
    fn split_conflict_appends_shared_prefix_to_both_sides() {
        // git's diff3 textual merge places matching JSON prefix/suffix OUTSIDE
        // the conflict markers (e.g. the shared `{`, `"version": 1,`, `}`
        // when notes are pretty-printed). Those lines belong to both sides
        // and must be threaded into both buffers so the JSON parses cleanly.
        let body = "{\n  \"version\": 1,\n<<<<<<< refs/notes/prompts\n  \"edits\": [{\"a\":1}]\n=======\n  \"edits\": [{\"b\":2}]\n>>>>>>> refs/notes/origin/prompts\n}\n";
        let (l, i) = split_conflict(body).expect("markers present");
        // Both sides should now reconstruct as parseable JSON-ish bodies
        // carrying the shared envelope around their own conflicting line.
        assert!(l.contains("\"version\": 1"));
        assert!(l.contains("[{\"a\":1}]"));
        assert!(l.trim_end().ends_with('}'));
        assert!(i.contains("\"version\": 1"));
        assert!(i.contains("[{\"b\":2}]"));
        assert!(i.trim_end().ends_with('}'));
    }

    #[test]
    fn merge_unions_disjoint_edits() {
        let local = Note::new(vec![edit("a", 0, Some("t1"), "2026-01-01T00:00:00Z", "h1")]);
        let incoming = Note::new(vec![edit("b", 5, Some("t2"), "2026-01-02T00:00:00Z", "h2")]);
        let local_json = local.to_json().unwrap();
        let incoming_json = incoming.to_json().unwrap();
        let merged = merge_note_pair(
            "deadbeef",
            "refs/notes/prompts",
            &local_json,
            &incoming_json,
        )
        .unwrap();
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
            "refs/notes/prompts",
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
        // Same conversation+turn+None tool_use_id but distinct (file,
        // line_range) pairs — these are distinct edits captured by an older
        // client and should not collapse. Mirrors the
        // `(conversation_id, turn_index, tool_use_id, fallback)` shape used
        // by `hook.rs::dedupe_and_sort_edits`, where the fallback is
        // `"{file}@{start}-{end}"` when `tool_use_id` is `None`.
        let local = Note::new(vec![edit_at(
            "a",
            0,
            None,
            "2026-01-01T00:00:00Z",
            "h1",
            "alpha.rs",
            [1, 1],
        )]);
        let incoming = Note::new(vec![edit_at(
            "a",
            0,
            None,
            "2026-01-02T00:00:00Z",
            "h2",
            "beta.rs",
            [1, 1],
        )]);
        let merged = merge_note_pair(
            "deadbeef",
            "refs/notes/prompts",
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
            "refs/notes/prompts",
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
        let err = merge_note_pair("deadbeef", "refs/notes/prompts", bad, &good).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("schema version") && msg.contains("v99"),
            "expected schema-version error mentioning v99, got: {msg}"
        );
    }

    #[test]
    fn merge_error_message_uses_target_ref_for_abort_hint() {
        // The user-facing abort hint should track whichever ref is actually
        // being merged, not the hardcoded `prompts` default — otherwise a
        // user resolving a custom notes ref (e.g. `prompts-private`) would be
        // told to abort the wrong ref.
        let bad = r#"{"version":99,"edits":[]}"#;
        let good = Note::new(vec![]).to_json().unwrap();
        let err =
            merge_note_pair("deadbeef", "refs/notes/prompts-private", bad, &good).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--ref=refs/notes/prompts-private"),
            "expected abort hint with the target ref, got: {msg}"
        );
    }

    #[test]
    fn parse_note_with_context_translates_missing_version() {
        // `MissingVersion` (e.g., truncated note, version field absent) gets
        // an explicit hint pointing at `git notes show <sha>` so the user
        // knows how to inspect the malformed payload.
        let body = r#"{"edits":[]}"#;
        let err =
            parse_note_with_context("deadbeef", "refs/notes/prompts", "local", body).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing a 'version' field"),
            "expected missing-version hint, got: {msg}"
        );
        assert!(
            msg.contains("git notes show deadbeef"),
            "expected inspection hint, got: {msg}"
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
