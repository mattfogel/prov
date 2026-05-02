//! Hook-event dispatch.
//!
//! Called by Claude Code hooks (`UserPromptSubmit`, `PostToolUse`, `Stop`,
//! `SessionStart`) and by git hooks (`post-commit`, `post-rewrite`, `pre-push`).
//!
//! **Defensive contract.** All hook subcommands always exit `0` — even on
//! internal error — and log to `<git-dir>/prov-staging/log`. A capture failure
//! must never block the agent loop nor the user's commit. The few branches
//! that intentionally block (e.g., U8's pre-push gate when an unredacted
//! secret is detected) live in dedicated handlers, not here.
//!
//! Each handler reads its hook payload from stdin (Claude Code's hook
//! contract) and runs `Redactor::redact` over any prompt-or-summary text
//! before staging. Even local-only staging is scrubbed: a future opt-in
//! `prov push` should never find raw secrets in the staging tree.
//!
//! The parsers below operate on the documented `tool_input` envelope
//! (Edit/Write/MultiEdit shapes per the Claude Code tool docs). Live-session
//! payloads were diffed against the fixtures and matched; the
//! `tool_response.structuredPatch` shape is still unparsed in v1 (we don't
//! need it — `tool_input` carries enough), so its undocumented status doesn't
//! gate capture today.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use serde::Deserialize;

use prov_core::git::{Git, GitError};
use prov_core::redactor::Redactor;
use prov_core::schema::{DerivedFrom, Edit, Note};
use prov_core::session::SessionId;
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::staging::{EditRecord, SessionMeta, Staging, StagingError, TurnRecord};
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

use super::common::CACHE_FILENAME;
use prov_core::time::now_iso8601;

#[derive(Parser, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub event: Event,
}

#[derive(Subcommand, Debug)]
pub enum Event {
    /// Claude Code: `UserPromptSubmit` — stage prompt + session metadata.
    UserPromptSubmit,
    /// Claude Code: `PostToolUse` matched on `Edit|Write|MultiEdit` — stage the edit.
    PostToolUse,
    /// Claude Code: `Stop` — mark the current turn complete.
    Stop,
    /// Claude Code: `SessionStart` — capture model name for this session.
    SessionStart,
    /// Git: `post-commit` — flush staged edits into a note attached to HEAD.
    PostCommit,
    /// Git: `post-rewrite` — reattach notes after amend/rebase/squash. Owned by U9.
    PostRewrite {
        /// `amend` or `rebase` — git passes this as the first arg.
        #[allow(dead_code)]
        kind: String,
    },
    /// Git: `pre-push` — scan notes refs for unredacted secrets before push. Owned by U8.
    PrePush,
}

/// Defensive entry point. Every error path here logs and exits 0 — the run
/// signature returns `anyhow::Result` only to match the rest of the CLI's
/// dispatch shape. The one exception is `pre-push`: when its secret-scanning
/// gate fires, the handler intentionally returns an error so the surrounding
/// `git push` aborts. Internal errors inside pre-push (malformed stdin, etc.)
/// still log and exit 0 — a Prov bug should never break a user's push.
#[allow(
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps,
    clippy::needless_return
)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let Ok(git) = Git::discover(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    else {
        return Ok(());
    };
    let staging = Staging::new(git.git_dir());

    if matches!(args.event, Event::PrePush) {
        return run_pre_push(&git, &staging);
    }

    let event_label = format!("{:?}", args.event);
    let result = match args.event {
        Event::UserPromptSubmit => handle_user_prompt_submit(&staging),
        Event::PostToolUse => handle_post_tool_use(&staging, Some(git.work_tree())),
        Event::Stop => handle_stop(&staging),
        Event::SessionStart => handle_session_start(&staging),
        Event::PostCommit => handle_post_commit(&git, &staging),
        // U9 owns post-rewrite. Land here as a no-op so the git hook script
        // can wire the command without breaking; U9 fills in real behaviour.
        Event::PostRewrite { .. } => Ok(()),
        Event::PrePush => unreachable!("pre-push routed above"),
    };

    if let Err(e) = result {
        let _ = staging.append_log(&format!(
            "{}: hook {event_label} failed: {e}",
            now_iso8601()
        ));
    }
    Ok(())
}

/// Pre-push wrapper. Translates the handler's `PrePushOutcome` into either
/// `Ok(())` (allow) or an `anyhow::Error` (block, which propagates to main as
/// a non-zero exit and aborts the surrounding `git push`).
fn run_pre_push(git: &Git, staging: &Staging) -> anyhow::Result<()> {
    match handle_pre_push(git) {
        Ok(PrePushOutcome::Allow) => Ok(()),
        Ok(PrePushOutcome::Block(reasons)) => {
            for reason in &reasons {
                eprintln!("{reason}");
            }
            eprintln!();
            eprintln!(
                "Re-run after fixing the secrets, or pass `--no-verify` to bypass \
                 the gate (audit-logged when used via `prov push --no-verify`)."
            );
            anyhow::bail!("prov pre-push: blocked by secret-scanning gate");
        }
        Err(e) => {
            let _ = staging.append_log(&format!("{}: hook pre-push failed: {e}", now_iso8601()));
            Ok(())
        }
    }
}

// =================================================================
// UserPromptSubmit
// =================================================================

#[derive(Debug, Deserialize)]
struct UserPromptSubmitPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
}

fn handle_user_prompt_submit(staging: &Staging) -> Result<(), HandlerError> {
    let payload: UserPromptSubmitPayload = read_stdin_json()?;
    let raw_session = payload.session_id.unwrap_or_default();
    let prompt = payload.prompt.unwrap_or_default();
    let Ok(sid) = SessionId::parse(raw_session) else {
        return Ok(());
    };

    // First/last-line `# prov:private` opt-out (case-insensitive). A
    // `# prov:private` inside a code-block paste does not flip the routing.
    let private = is_prov_private(&prompt);

    // Redact even staged content. The redactor is the primary defense; pre-push
    // (U8) is the second line.
    let redactor = Redactor::new();
    let redacted = redactor.redact(&prompt);

    let turn_index = staging.count_turns(&sid, private)?;
    let record = TurnRecord {
        session_id: sid.as_str().to_string(),
        turn_index,
        prompt: redacted.text,
        private,
        transcript_path: payload.transcript_path,
        cwd: payload.cwd,
        started_at: now_iso8601(),
        completed_at: None,
    };
    staging.write_turn(&sid, private, turn_index, &record)?;
    Ok(())
}

/// True when the prompt's first or last line is the magic phrase
/// `# prov:private` (case-insensitive). Restricted to first/last lines so a
/// paste of code that contains `# prov:private` inside a code block does not
/// silently flip the privacy bit.
fn is_prov_private(prompt: &str) -> bool {
    let lines: Vec<&str> = prompt.lines().collect();
    if lines.first().is_some_and(|l| line_is_prov_private(l)) {
        return true;
    }
    lines.last().is_some_and(|l| line_is_prov_private(l))
}

fn line_is_prov_private(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(rest) = trimmed.strip_prefix('#') else {
        return false;
    };
    rest.trim().eq_ignore_ascii_case("prov:private")
}

// =================================================================
// PostToolUse
// =================================================================

#[derive(Debug, Deserialize)]
struct PostToolUsePayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_input: serde_json::Value,
    #[serde(default)]
    tool_response: serde_json::Value,
}

fn handle_post_tool_use(staging: &Staging, work_tree: Option<&Path>) -> Result<(), HandlerError> {
    let payload: PostToolUsePayload = read_stdin_json()?;
    let raw_session = payload.session_id.unwrap_or_default();
    let Ok(sid) = SessionId::parse(raw_session) else {
        return Ok(());
    };
    let tool_name = payload.tool_name.unwrap_or_default();
    if !matches!(tool_name.as_str(), "Edit" | "Write" | "MultiEdit") {
        return Ok(());
    }

    // Public/private routing: use the most-recent turn's `private` flag.
    let private = current_turn_is_private(staging, &sid);
    let turn_index = staging
        .count_turns(&sid, private)
        .unwrap_or(0)
        .saturating_sub(1);

    let edits = decompose_tool_use(
        &tool_name,
        &payload.tool_input,
        &payload.tool_response,
        &sid,
        turn_index,
        payload.tool_use_id.as_deref(),
        work_tree,
    );

    for edit in edits {
        if let Err(e) = staging.append_edit(&sid, private, &edit) {
            staging
                .append_log(&format!("{}: append_edit failed: {e}", now_iso8601()))
                .ok();
        }
    }
    Ok(())
}

fn current_turn_is_private(staging: &Staging, sid: &SessionId) -> bool {
    let last_public = most_recent_turn_started_at(staging, sid, false);
    let last_private = most_recent_turn_started_at(staging, sid, true);
    match (last_public, last_private) {
        (Some(p), Some(pr)) => pr > p,
        (None, Some(_)) => true,
        _ => false,
    }
}

/// Find the most recent `turn-<N>.json` in the (public or private) session
/// directory and return its `started_at` field. Avoids reading every turn
/// just to compare timestamps — we only need the last one in either subtree.
fn most_recent_turn_started_at(
    staging: &Staging,
    sid: &SessionId,
    private: bool,
) -> Option<String> {
    let dir = staging.session_dir(sid, private);
    if !dir.exists() {
        return None;
    }
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut best: Option<(u32, std::path::PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        let Some(num) = s
            .strip_prefix("turn-")
            .and_then(|s| s.strip_suffix(".json"))
        else {
            continue;
        };
        let Ok(n) = num.parse::<u32>() else { continue };
        let bigger = match best.as_ref() {
            None => true,
            Some((b, _)) => n > *b,
        };
        if bigger {
            best = Some((n, entry.path()));
        }
    }
    let (_, path) = best?;
    let bytes = std::fs::read(&path).ok()?;
    let rec: TurnRecord = serde_json::from_slice(&bytes).ok()?;
    Some(rec.started_at)
}

/// Decompose one PostToolUse payload into one or more `EditRecord`s.
///
/// Pinned to the documented Claude Code tool inputs:
/// - `Edit { file_path, old_string, new_string, replace_all? }`
/// - `Write { file_path, content }`
/// - `MultiEdit { file_path, edits: [{old_string, new_string, replace_all?}, ...] }`
///
/// `tool_response.structuredPatch` is not parsed in v1. The empirical-pinning
/// step is tracked as a follow-up after a live-session capture.
fn decompose_tool_use(
    tool_name: &str,
    tool_input: &serde_json::Value,
    _tool_response: &serde_json::Value,
    sid: &SessionId,
    turn_index: u32,
    tool_use_id: Option<&str>,
    work_tree: Option<&Path>,
) -> Vec<EditRecord> {
    let timestamp = now_iso8601();
    match tool_name {
        "Edit" => {
            let Some(file) = tool_input.get("file_path").and_then(|v| v.as_str()) else {
                return Vec::new();
            };
            let old = tool_input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = tool_input
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            vec![record_for(
                sid,
                turn_index,
                tool_name,
                tool_use_id,
                file,
                old,
                new,
                &timestamp,
                work_tree,
            )]
        }
        "Write" => {
            let Some(file) = tool_input.get("file_path").and_then(|v| v.as_str()) else {
                return Vec::new();
            };
            let content = tool_input
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            vec![record_for(
                sid,
                turn_index,
                tool_name,
                tool_use_id,
                file,
                "",
                content,
                &timestamp,
                work_tree,
            )]
        }
        "MultiEdit" => {
            let Some(file) = tool_input.get("file_path").and_then(|v| v.as_str()) else {
                return Vec::new();
            };
            let Some(edits) = tool_input.get("edits").and_then(|v| v.as_array()) else {
                return Vec::new();
            };
            edits
                .iter()
                .map(|e| {
                    let old = e.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
                    let new = e.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                    record_for(
                        sid,
                        turn_index,
                        tool_name,
                        tool_use_id,
                        file,
                        old,
                        new,
                        &timestamp,
                        work_tree,
                    )
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn record_for(
    sid: &SessionId,
    turn_index: u32,
    tool_name: &str,
    tool_use_id: Option<&str>,
    file: &str,
    before: &str,
    after: &str,
    timestamp: &str,
    work_tree: Option<&Path>,
) -> EditRecord {
    let after_lines: Vec<&str> = after.split('\n').collect();
    let line_count = u32::try_from(after_lines.len()).unwrap_or(u32::MAX);
    // Pre-commit we don't yet know the file's final line range. The
    // post-commit handler reconciles via diff matching, so capture-side range
    // is a best-effort placeholder of `[1, line_count]`.
    let line_range = [1_u32, line_count.max(1)];
    let content_hashes: Vec<String> = after_lines
        .iter()
        .map(|l| blake3::hash(l.as_bytes()).to_hex().to_string())
        .collect();
    EditRecord {
        session_id: sid.as_str().to_string(),
        turn_index,
        tool_name: tool_name.to_string(),
        tool_use_id: tool_use_id.map(str::to_string),
        file: relativize_for_storage(file, work_tree),
        line_range,
        before: before.to_string(),
        after: after.to_string(),
        content_hashes,
        timestamp: timestamp.to_string(),
    }
}

/// Convert Claude Code's absolute `file_path` to a path relative to the work
/// tree. Storing relative paths means the staged record's `file` field matches
/// `git diff` output directly (which always emits paths relative to the
/// repo root) and is portable across machines with different repo locations.
///
/// Falls back to the input string when the path is already relative, lies
/// outside the work tree, or no work tree was provided (test paths).
fn relativize_for_storage(file: &str, work_tree: Option<&Path>) -> String {
    let p = Path::new(file);
    if !p.is_absolute() {
        return file.to_string();
    }
    let Some(wt) = work_tree else {
        return file.to_string();
    };
    if let Ok(rel) = p.strip_prefix(wt) {
        return rel.to_string_lossy().into_owned();
    }
    // Fallback: symlinks can put the captured `file_path` and the work tree
    // in different forms (notably macOS `/var` → `/private/var`, which trips
    // up TempDir-based tests, and could hit users whose repos live behind a
    // symlinked mount). Canonicalize both sides before giving up.
    if let (Ok(file_canon), Ok(wt_canon)) = (p.canonicalize(), wt.canonicalize()) {
        if let Ok(rel) = file_canon.strip_prefix(&wt_canon) {
            return rel.to_string_lossy().into_owned();
        }
    }
    file.to_string()
}

// =================================================================
// Stop
// =================================================================

#[derive(Debug, Deserialize)]
struct StopPayload {
    #[serde(default)]
    session_id: Option<String>,
}

fn handle_stop(staging: &Staging) -> Result<(), HandlerError> {
    let payload: StopPayload = read_stdin_json()?;
    let Ok(sid) = SessionId::parse(payload.session_id.unwrap_or_default()) else {
        return Ok(());
    };
    finalize_last_turn(staging, &sid, false);
    finalize_last_turn(staging, &sid, true);
    Ok(())
}

fn finalize_last_turn(staging: &Staging, sid: &SessionId, private: bool) {
    let Ok(mut turns) = staging.read_turns(sid, private) else {
        return;
    };
    let Some(last) = turns.pop() else {
        return;
    };
    let mut updated = last.clone();
    if updated.completed_at.is_none() {
        updated.completed_at = Some(now_iso8601());
        let _ = staging.write_turn(sid, private, updated.turn_index, &updated);
    }
}

// =================================================================
// SessionStart
// =================================================================

#[derive(Debug, Deserialize)]
struct SessionStartPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

fn handle_session_start(staging: &Staging) -> Result<(), HandlerError> {
    let payload: SessionStartPayload = read_stdin_json()?;
    let Ok(sid) = SessionId::parse(payload.session_id.unwrap_or_default()) else {
        return Ok(());
    };
    let model = payload.model.unwrap_or_else(|| "unknown".to_string());
    let meta = SessionMeta {
        session_id: sid.as_str().to_string(),
        model,
        started_at: now_iso8601(),
    };
    staging.write_session_meta(&sid, &meta)?;
    Ok(())
}

// =================================================================
// post-commit (git hook)
// =================================================================

fn handle_post_commit(git: &Git, staging: &Staging) -> Result<(), HandlerError> {
    let head = git
        .capture(["rev-parse", "HEAD"])
        .map_err(HandlerError::Git)?
        .trim()
        .to_string();

    let cherry_pick_source = read_cherry_pick_head(git);
    let added_by_file = collect_added_lines(git, &head)?;

    let sessions = staging.list_sessions().unwrap_or_default();
    // Public and private staged edits flush to separate notes refs (the latter
    // is local-only and never reaches a remote). Run the matcher twice — once
    // per visibility — so a session that interleaves public and private turns
    // routes each batch to the correct ref.
    let public_matched = flush_visibility(
        git,
        staging,
        &sessions,
        &added_by_file,
        cherry_pick_source.as_deref(),
        false,
    );
    let private_matched = flush_visibility(
        git,
        staging,
        &sessions,
        &added_by_file,
        cherry_pick_source.as_deref(),
        true,
    );

    if !public_matched.edits.is_empty() {
        write_note_and_cache(
            git,
            staging,
            &head,
            public_matched.edits,
            NOTES_REF_PUBLIC,
            /* stamp_ref_sha = */ true,
        );
    }
    if !private_matched.edits.is_empty() {
        write_note_and_cache(
            git,
            staging,
            &head,
            private_matched.edits,
            NOTES_REF_PRIVATE,
            /* stamp_ref_sha = */ false,
        );
    }

    // Cleanup: a session is fully done only when both public and private
    // staging trees have no unmatched edits left. Removing the session dir
    // earlier would lose any private edits still waiting on a future commit.
    for sid in &sessions {
        let pub_total = staging.read_edits(sid, false).unwrap_or_default().len();
        let priv_total = staging.read_edits(sid, true).unwrap_or_default().len();
        let pub_matched = public_matched
            .keys
            .iter()
            .filter(|(s, _, _)| s == sid)
            .count();
        let priv_matched = private_matched
            .keys
            .iter()
            .filter(|(s, _, _)| s == sid)
            .count();
        let total = pub_total + priv_total;
        if total > 0 && (pub_matched + priv_matched) == total {
            staging.remove_session(sid).ok();
        }
    }

    Ok(())
}

#[derive(Default)]
struct VisibilityMatches {
    edits: Vec<Edit>,
    /// (session, file, line-range) tuples for cleanup accounting.
    keys: Vec<(SessionId, String, [u32; 2])>,
}

fn flush_visibility(
    git: &Git,
    staging: &Staging,
    sessions: &[SessionId],
    added_by_file: &std::collections::HashMap<String, Vec<AddedLine>>,
    cherry_pick_source: Option<&str>,
    private: bool,
) -> VisibilityMatches {
    let mut out = VisibilityMatches::default();
    for sid in sessions {
        let session_meta = staging
            .read_session_meta(sid)
            .ok()
            .flatten()
            .unwrap_or_else(|| SessionMeta {
                session_id: sid.as_str().to_string(),
                model: "unknown".into(),
                started_at: String::new(),
            });
        let turns = staging.read_turns(sid, private).unwrap_or_default();
        let edits = staging.read_edits(sid, private).unwrap_or_default();
        for er in &edits {
            let Some(matched) = match_edit_to_diff(er, added_by_file, Some(git.work_tree())) else {
                continue;
            };
            let prompt = turns
                .iter()
                .find(|t| t.turn_index == er.turn_index)
                .map(|t| t.prompt.clone())
                .unwrap_or_default();
            let mut edit = Edit {
                file: matched.file.clone(),
                line_range: matched.line_range,
                content_hashes: er.content_hashes.clone(),
                original_blob_sha: None,
                prompt,
                conversation_id: er.session_id.clone(),
                turn_index: er.turn_index,
                tool_use_id: er.tool_use_id.clone(),
                preceding_turns_summary: None,
                model: session_meta.model.clone(),
                tool: "claude-code".into(),
                timestamp: er.timestamp.clone(),
                derived_from: None,
            };
            // Cherry-pick: stamp every matched edit with `derived_from: Rewrite`
            // pointing back to the source commit. v1 keeps this coarse; U9 owns
            // the precise per-edit `source_edit` index pinning.
            if let Some(source) = cherry_pick_source {
                edit.derived_from = Some(DerivedFrom::Rewrite {
                    source_commit: source.to_string(),
                    source_edit: 0,
                });
            }
            out.edits.push(edit);
            out.keys
                .push((sid.clone(), er.file.clone(), matched.line_range));
        }
    }
    out
}

/// Write `edits` as a note attached to `head` on `ref_name`, then update the
/// SQLite cache. Cache failures are logged and swallowed — the post-commit
/// hook must never propagate errors out (`prov reindex` recovers cache state).
fn write_note_and_cache(
    git: &Git,
    staging: &Staging,
    head: &str,
    edits: Vec<Edit>,
    ref_name: &str,
    stamp_ref_sha: bool,
) {
    let note = Note::new(edits);
    let store = NotesStore::new(git.clone(), ref_name);
    if let Err(e) = store.write(head, &note) {
        staging
            .append_log(&format!(
                "{}: notes.write({ref_name}) failed: {e}",
                now_iso8601()
            ))
            .ok();
        return;
    }

    let cache_path = git.git_dir().join(CACHE_FILENAME);
    if !cache_path.exists() {
        return;
    }
    let mut cache = match Cache::open(&cache_path) {
        Ok(c) => c,
        Err(e) => {
            staging
                .append_log(&format!("{}: cache.open failed: {e}", now_iso8601()))
                .ok();
            return;
        }
    };
    let result = if stamp_ref_sha {
        let new_ref_sha = store.ref_sha().ok().flatten();
        cache.upsert_note(head, &note, new_ref_sha.as_deref())
    } else {
        cache.upsert_note_no_stamp(head, &note)
    };
    if let Err(e) = result {
        staging
            .append_log(&format!(
                "{}: cache.upsert_note({ref_name}) failed: {e}",
                now_iso8601()
            ))
            .ok();
    }
}

#[derive(Debug)]
struct Match {
    /// File path as it appears in `git diff` (relative to the work tree). The
    /// Edit stored in the note carries this normalized form so the resolver
    /// and CLI consumers compare paths consistently regardless of whether
    /// the original staged record used an absolute or relative path.
    file: String,
    line_range: [u32; 2],
}

fn match_edit_to_diff(
    er: &EditRecord,
    added_by_file: &std::collections::HashMap<String, Vec<AddedLine>>,
    work_tree: Option<&Path>,
) -> Option<Match> {
    // Normalize the staged path to the same shape `git diff` emits. Required
    // because Claude Code's tool_input passes absolute paths (we relativize at
    // staging time, but pre-fix data may still be absolute) while the diff
    // output is always relative to the repo root.
    let key = relativize_for_storage(&er.file, work_tree);
    let added = added_by_file.get(&key)?;
    if added.is_empty() {
        return None;
    }

    // Strategy a: exact match — every captured `after` line appears verbatim
    // as a contiguous run in the diff's added lines.
    let added_content: Vec<String> = added.iter().map(|l| l.content.clone()).collect();
    let after_lines: Vec<&str> = er.after.split('\n').collect();
    if let Some(range) = exact_window_match_str(&after_lines, &added_content, added) {
        return Some(Match {
            file: key,
            line_range: range,
        });
    }

    // Strategy b: normalized — strip trailing whitespace, collapse internal
    // runs, normalize ASCII quote style. Tolerates prettier/black/rustfmt
    // running between PostToolUse and the commit.
    let norm_after: Vec<String> = after_lines.iter().map(|l| normalize(l)).collect();
    let norm_added: Vec<String> = added_content.iter().map(|l| normalize(l)).collect();
    if let Some(range) = exact_window_match_norm(&norm_after, &norm_added, added) {
        return Some(Match {
            file: key,
            line_range: range,
        });
    }

    // Strategy c: line-range proximity — capture's `[start, end]` overlaps
    // any added-line window for this file by ≥ 50%.
    let captured = er.line_range;
    let captured_len = u32::from(captured[1] >= captured[0])
        * (captured[1].saturating_sub(captured[0]).saturating_add(1));
    if let Some(window) = added_window(added) {
        let overlap = window_overlap(captured, window);
        if captured_len > 0 && overlap * 2 >= captured_len {
            return Some(Match {
                file: key,
                line_range: window,
            });
        }
    }
    None
}

fn exact_window_match_str(
    after: &[&str],
    added: &[String],
    added_meta: &[AddedLine],
) -> Option<[u32; 2]> {
    let needle: Vec<&&str> = after.iter().filter(|l| !l.is_empty()).collect();
    if needle.is_empty() || added.is_empty() || needle.len() > added.len() {
        return None;
    }
    for start in 0..=added.len() - needle.len() {
        if added[start..start + needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a == **b)
        {
            let first = added_meta[start].line_no;
            let last = added_meta[start + needle.len() - 1].line_no;
            return Some([first, last]);
        }
    }
    None
}

fn exact_window_match_norm(
    after: &[String],
    added: &[String],
    added_meta: &[AddedLine],
) -> Option<[u32; 2]> {
    let needle: Vec<&String> = after.iter().filter(|l| !l.is_empty()).collect();
    if needle.is_empty() || added.is_empty() || needle.len() > added.len() {
        return None;
    }
    for start in 0..=added.len() - needle.len() {
        if added[start..start + needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a == *b)
        {
            let first = added_meta[start].line_no;
            let last = added_meta[start + needle.len() - 1].line_no;
            return Some([first, last]);
        }
    }
    None
}

fn added_window(added: &[AddedLine]) -> Option<[u32; 2]> {
    let first = added.first()?.line_no;
    let last = added.last()?.line_no;
    Some([first, last])
}

fn window_overlap(a: [u32; 2], b: [u32; 2]) -> u32 {
    let start = a[0].max(b[0]);
    let end = a[1].min(b[1]);
    if start <= end {
        end - start + 1
    } else {
        0
    }
}

fn normalize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last_was_space = false;
    for c in line.trim_end().chars() {
        let c = match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            _ => c,
        };
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out
}

#[derive(Debug, Clone)]
struct AddedLine {
    line_no: u32,
    content: String,
}

fn collect_added_lines(
    git: &Git,
    head: &str,
) -> Result<std::collections::HashMap<String, Vec<AddedLine>>, HandlerError> {
    let parent_count = git
        .capture(["rev-list", "--count", &format!("{head}^@")])
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    let raw = if parent_count == 0 {
        // Empty-tree SHA is well-known. Use it for the initial commit so the
        // diff still reports every line as `+`.
        let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        git.capture(["diff", "-U0", empty_tree, head])
            .map_err(HandlerError::Git)?
    } else {
        git.capture(["diff", "-U0", &format!("{head}~1..{head}")])
            .map_err(HandlerError::Git)?
    };
    Ok(parse_unified_diff_added(&raw))
}

fn parse_unified_diff_added(raw: &str) -> std::collections::HashMap<String, Vec<AddedLine>> {
    use std::collections::HashMap;
    let mut out: HashMap<String, Vec<AddedLine>> = HashMap::new();
    let mut current_file: Option<String> = None;
    let mut next_line_no: u32 = 0;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            current_file = Some(rest.to_string());
            continue;
        }
        // Real diff header lines are exactly `+++ ` or `--- ` followed by a
        // path (or `/dev/null`). We must not skip content lines whose body
        // happens to start with `+++` or `---` (e.g., a Markdown rule, or the
        // diff itself being captured as text).
        if is_diff_header(line) {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            // @@ -<a>,<b> +<c>,<d> @@
            let plus = rest
                .split_whitespace()
                .find(|t| t.starts_with('+'))
                .unwrap_or("+0");
            let trimmed = plus.trim_start_matches('+');
            let start = trimmed
                .split(',')
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0);
            next_line_no = start;
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            if let Some(file) = &current_file {
                out.entry(file.clone()).or_default().push(AddedLine {
                    line_no: next_line_no,
                    content: rest.to_string(),
                });
                next_line_no = next_line_no.saturating_add(1);
            }
        } else if line.starts_with('-') {
            // removed; doesn't advance the new-side counter
        } else if !line.starts_with("@@") && current_file.is_some() {
            // context line — rare with -U0, but advance if it shows up.
            next_line_no = next_line_no.saturating_add(1);
        }
    }
    out
}

/// Distinguish a real `--- a/...` / `+++ b/...` (or `/dev/null`) diff header
/// from a content line whose body just happens to start with `+++` or `---`
/// (e.g., an added Markdown horizontal rule). A real header always has a
/// space after the prefix; content `+++`/`---` do not.
fn is_diff_header(line: &str) -> bool {
    line.starts_with("+++ ") || line.starts_with("--- ")
}

fn read_cherry_pick_head(git: &Git) -> Option<String> {
    let path = git.git_dir().join("CHERRY_PICK_HEAD");
    let s = std::fs::read_to_string(path).ok()?;
    Some(s.trim().to_string())
}

// =================================================================
// pre-push (git hook)
// =================================================================

/// All-zero SHA used by `git push` stdin to mean "no ref" (deletion when in the
/// local-sha slot, or new ref when in the remote-sha slot).
const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

/// Outcome of `handle_pre_push`. Allow lets the push proceed; Block carries the
/// user-facing reason lines that get printed to stderr before `git push` aborts.
#[derive(Debug)]
enum PrePushOutcome {
    Allow,
    Block(Vec<String>),
}

/// Pre-push gate.
///
/// Reads stdin per `githooks(5)`: each line is `<local-ref> <local-sha>
/// <remote-ref> <remote-sha>`. For each line:
///
/// 1. Block if either side names `refs/notes/prompts-private`. Private notes
///    are local-only; prevent the manual-mapping bypass
///    (`git push origin refs/notes/prompts-private:refs/notes/prompts`) by
///    matching on both ref slots.
/// 2. Skip lines whose local ref is not `refs/notes/prompts` — the gate is
///    scoped to notes pushes by default per R6 (the alternative,
///    `prov.scanAllPushes`, is reserved for v1.x).
/// 3. For each note blob that is new or modified vs the remote tip, run the
///    redactor over its prompt and summary fields. Any detector hit blocks
///    the push.
fn handle_pre_push(git: &Git) -> Result<PrePushOutcome, HandlerError> {
    // Hard cap on pre-push payload size, matching read_stdin_json's defense
    // against a runaway producer piping gigabytes into the handler. The git
    // pre-push contract emits one ref-update line per pushed ref; even a
    // mass push is at most a few KB.
    const MAX_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

    let mut buf = String::new();
    let bytes_read = io::stdin()
        .take(MAX_PAYLOAD_BYTES + 1)
        .read_to_string(&mut buf)
        .map_err(|e| HandlerError::Stdin(e.to_string()))?;
    if u64::try_from(bytes_read).unwrap_or(u64::MAX) > MAX_PAYLOAD_BYTES {
        return Err(HandlerError::Stdin(format!(
            "pre-push payload exceeded {MAX_PAYLOAD_BYTES} bytes"
        )));
    }

    let mut blocks: Vec<String> = Vec::new();
    let redactor = Redactor::new();

    for raw in buf.lines() {
        let mut parts = raw.split_whitespace();
        let (Some(local_ref), Some(local_sha), Some(remote_ref), Some(remote_sha)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            // Malformed line — skip rather than fail. Defensive: a Prov bug
            // here must not break the user's push.
            continue;
        };

        // (1) Private-ref guard. Match on both slots so a manual mapping like
        // `git push origin refs/notes/prompts-private:refs/notes/prompts` is
        // also blocked — otherwise the user would route private content onto
        // the public remote ref and the rest of the gate would never see it.
        if local_ref == NOTES_REF_PRIVATE || remote_ref == NOTES_REF_PRIVATE {
            blocks.push(format!(
                "prov pre-push: refusing to push {NOTES_REF_PRIVATE} \
                 (mapping {local_ref} → {remote_ref}); private notes are local-only"
            ));
            continue;
        }

        // (2) Default scoping: only scan the public notes ref.
        if local_ref != NOTES_REF_PUBLIC {
            continue;
        }

        // (3) Skip deletions of the public ref — there is no new content to scan.
        if local_sha == ZERO_SHA {
            continue;
        }

        let new_blobs = diff_note_blobs(git, local_sha, remote_sha)?;

        for (commit_sha, blob_sha) in new_blobs {
            let Ok(content) = git.capture_bytes(["cat-file", "blob", &blob_sha]) else {
                continue;
            };
            let Ok(text) = String::from_utf8(content) else {
                continue;
            };
            // Parse as a note so we scan only prompt + summary text. Running
            // the redactor over the whole blob would fire on JSON metadata
            // (timestamps, model names) and produce false positives.
            let Ok(note) = prov_core::schema::Note::from_json(&text) else {
                continue;
            };

            let mut hit_kinds: Vec<String> = Vec::new();
            for edit in &note.edits {
                let r = redactor.redact(&edit.prompt);
                hit_kinds.extend(r.redactions.iter().map(|x| x.kind.as_marker()));
                if let Some(s) = &edit.preceding_turns_summary {
                    let r = redactor.redact(s);
                    hit_kinds.extend(r.redactions.iter().map(|x| x.kind.as_marker()));
                }
            }
            if !hit_kinds.is_empty() {
                hit_kinds.sort();
                hit_kinds.dedup();
                blocks.push(format!(
                    "prov pre-push: detected unredacted secret(s) in note for commit {commit_sha}: \
                     [{}]",
                    hit_kinds.join(", ")
                ));
            }
        }
    }

    if blocks.is_empty() {
        Ok(PrePushOutcome::Allow)
    } else {
        Ok(PrePushOutcome::Block(blocks))
    }
}

/// List `(commit_sha, blob_sha)` pairs for note blobs that exist in `local_sha`
/// but are absent or different in `remote_sha`. When `remote_sha` is the
/// all-zero SHA (new ref), every local note is "new".
fn diff_note_blobs(
    git: &Git,
    local_sha: &str,
    remote_sha: &str,
) -> Result<Vec<(String, String)>, HandlerError> {
    let local = list_note_blobs(git, local_sha)?;
    let remote = if remote_sha == ZERO_SHA {
        Vec::new()
    } else {
        list_note_blobs(git, remote_sha)?
    };
    let remote_map: std::collections::HashMap<String, String> = remote.into_iter().collect();

    let mut out = Vec::new();
    for (path, blob_sha) in local {
        match remote_map.get(&path) {
            Some(rsha) if rsha == &blob_sha => {} // unchanged
            _ => out.push((path, blob_sha)),
        }
    }
    Ok(out)
}

/// Walk a notes-ref tip's tree and return `(annotated_commit_sha, blob_sha)`
/// for every note blob. Strips fanout slashes from the path so the returned
/// commit-sha matches what `git rev-parse` produces. Tree entries whose path
/// does not collapse to a 40-char hex SHA are skipped — printing a bogus
/// label inside the gate's block message confuses both humans and agents.
fn list_note_blobs(git: &Git, commit_sha: &str) -> Result<Vec<(String, String)>, HandlerError> {
    let raw = git
        .capture(["ls-tree", "-r", commit_sha])
        .map_err(HandlerError::Git)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split_whitespace();
        let _mode = parts.next();
        let typ = parts.next();
        let sha = parts.next();
        if typ != Some("blob") {
            continue;
        }
        let Some(sha) = sha else { continue };
        let normalized = path.replace('/', "");
        if !is_full_hex_sha(&normalized) {
            continue;
        }
        out.push((normalized, sha.to_string()));
    }
    Ok(out)
}

fn is_full_hex_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// =================================================================
// shared helpers
// =================================================================

#[derive(Debug, thiserror::Error)]
enum HandlerError {
    #[error(transparent)]
    Staging(#[from] StagingError),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    Git(GitError),
    #[error("stdin read failed: {0}")]
    Stdin(String),
}

fn read_stdin_json<T: for<'de> Deserialize<'de>>() -> Result<T, HandlerError> {
    /// Hard cap on hook-payload size. Real Claude Code hook payloads are tiny
    /// (a few KB at most); the cap defends against a runaway agent piping
    /// gigabytes into the handler and OOM-ing the commit.
    const MAX_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

    let mut buf = String::new();
    // Read one byte past the cap so we can distinguish "exactly at the cap" from
    // "over the cap" without a separate length probe.
    let bytes_read = io::stdin()
        .take(MAX_PAYLOAD_BYTES + 1)
        .read_to_string(&mut buf)
        .map_err(|e| HandlerError::Stdin(e.to_string()))?;
    if u64::try_from(bytes_read).unwrap_or(u64::MAX) > MAX_PAYLOAD_BYTES {
        return Err(HandlerError::Stdin(format!(
            "hook payload exceeded {MAX_PAYLOAD_BYTES} bytes"
        )));
    }
    if buf.trim().is_empty() {
        // Treat empty stdin as "{}" so handlers fall through to their default
        // (which is typically "do nothing"). Lets the git-hook wrapper invoke
        // these without a payload during scaffolding.
        buf = "{}".to_string();
    }
    serde_json::from_str(&buf).map_err(HandlerError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prov_private_first_line_only() {
        assert!(is_prov_private("# prov:private\nfoo bar"));
        assert!(is_prov_private("foo\n# prov:private"));
        assert!(is_prov_private("# Prov:Private\nfoo"));
        assert!(is_prov_private("# PROV:PRIVATE"));
        assert!(is_prov_private("foo\n# PROV:PRIVATE"));
        // Inside the body but not first/last line — does NOT trigger.
        assert!(!is_prov_private("foo\n# prov:private\nbar"));
        // Substring inside text — no trigger.
        assert!(!is_prov_private("write a parser for # prov:private syntax"));
    }

    #[test]
    fn decompose_edit_produces_one_record() {
        let sid = SessionId::parse("sess_t").unwrap();
        let input = serde_json::json!({
            "file_path": "src/lib.rs",
            "old_string": "old",
            "new_string": "new",
        });
        let recs = decompose_tool_use(
            "Edit",
            &input,
            &serde_json::Value::Null,
            &sid,
            0,
            Some("toolu_1"),
            None,
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].tool_name, "Edit");
        assert_eq!(recs[0].file, "src/lib.rs");
        assert_eq!(recs[0].after, "new");
    }

    #[test]
    fn decompose_relativizes_absolute_file_path_under_work_tree() {
        // Real Claude Code passes absolute file paths in tool_input. Storing
        // them verbatim made matching against `git diff` (which uses paths
        // relative to the repo root) miss every time. Staging records must
        // hold the relative form.
        let sid = SessionId::parse("sess_t").unwrap();
        let work_tree = Path::new("/tmp/repo");
        let input = serde_json::json!({
            "file_path": "/tmp/repo/src/lib.rs",
            "old_string": "old",
            "new_string": "new",
        });
        let recs = decompose_tool_use(
            "Edit",
            &input,
            &serde_json::Value::Null,
            &sid,
            0,
            Some("toolu_1"),
            Some(work_tree),
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].file, "src/lib.rs");
    }

    #[test]
    fn decompose_keeps_absolute_path_outside_work_tree() {
        // Defensive: an edit to a file outside the repo (rare, but possible
        // via tool misuse) shouldn't get silently rewritten into a confusing
        // pseudo-relative path.
        let sid = SessionId::parse("sess_t").unwrap();
        let work_tree = Path::new("/tmp/repo");
        let input = serde_json::json!({
            "file_path": "/etc/hosts",
            "content": "anything",
        });
        let recs = decompose_tool_use(
            "Write",
            &input,
            &serde_json::Value::Null,
            &sid,
            0,
            None,
            Some(work_tree),
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].file, "/etc/hosts");
    }

    #[test]
    fn decompose_multiedit_produces_one_record_per_inner_edit() {
        let sid = SessionId::parse("sess_t").unwrap();
        let input = serde_json::json!({
            "file_path": "src/lib.rs",
            "edits": [
                { "old_string": "a", "new_string": "1" },
                { "old_string": "b", "new_string": "2" },
                { "old_string": "c", "new_string": "3" },
            ],
        });
        let recs = decompose_tool_use(
            "MultiEdit",
            &input,
            &serde_json::Value::Null,
            &sid,
            0,
            Some("toolu_1"),
            None,
        );
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].after, "1");
        assert_eq!(recs[2].after, "3");
    }

    #[test]
    fn decompose_unknown_tool_is_empty() {
        let sid = SessionId::parse("sess_t").unwrap();
        let recs = decompose_tool_use(
            "SomethingElse",
            &serde_json::Value::Null,
            &serde_json::Value::Null,
            &sid,
            0,
            None,
            None,
        );
        assert!(recs.is_empty());
    }

    #[test]
    fn match_edit_to_diff_normalizes_legacy_absolute_paths() {
        // Pre-fix staging records carry absolute file paths. The matcher
        // backstop must strip the work-tree prefix before doing the diff
        // lookup so existing data captured before the fix can still be
        // flushed without forcing the user to re-run their session.
        use std::collections::HashMap;
        let work_tree = Path::new("/tmp/repo");
        let mut added_by_file: HashMap<String, Vec<AddedLine>> = HashMap::new();
        added_by_file.insert(
            "src/lib.rs".to_string(),
            vec![
                AddedLine {
                    line_no: 1,
                    content: "alpha".into(),
                },
                AddedLine {
                    line_no: 2,
                    content: "beta".into(),
                },
            ],
        );
        let er = EditRecord {
            session_id: "s".into(),
            turn_index: 0,
            tool_name: "Write".into(),
            tool_use_id: None,
            file: "/tmp/repo/src/lib.rs".into(),
            line_range: [1, 2],
            before: String::new(),
            after: "alpha\nbeta\n".into(),
            content_hashes: vec![],
            timestamp: "2026-01-01T00:00:00Z".into(),
        };
        let m = match_edit_to_diff(&er, &added_by_file, Some(work_tree)).expect("match");
        assert_eq!(m.file, "src/lib.rs");
        assert_eq!(m.line_range, [1, 2]);
    }

    #[test]
    fn parse_unified_diff_extracts_added_lines() {
        let raw = "diff --git a/src/lib.rs b/src/lib.rs\n\
                   --- a/src/lib.rs\n\
                   +++ b/src/lib.rs\n\
                   @@ -0,0 +1,3 @@\n\
                   +alpha\n\
                   +beta\n\
                   +gamma\n";
        let map = parse_unified_diff_added(raw);
        let lines = map.get("src/lib.rs").unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line_no, 1);
        assert_eq!(lines[0].content, "alpha");
        assert_eq!(lines[2].line_no, 3);
    }

    #[test]
    fn parse_unified_diff_keeps_added_lines_starting_with_plus_plus_plus() {
        // A captured diff that itself contains lines starting with `+++` or
        // `---` (e.g., an embedded diff snippet, or a Markdown horizontal rule)
        // must NOT be mis-classified as a header and dropped.
        let raw = "diff --git a/notes.md b/notes.md\n\
                   --- a/notes.md\n\
                   +++ b/notes.md\n\
                   @@ -0,0 +1,3 @@\n\
                   ++++hi\n\
                   +---hello\n\
                   +regular\n";
        let map = parse_unified_diff_added(raw);
        let lines = map.get("notes.md").unwrap();
        assert_eq!(lines.len(), 3);
        // Strips exactly one `+` (the diff marker); the body's `+++hi` /
        // `---hello` payload is preserved.
        assert_eq!(lines[0].content, "+++hi");
        assert_eq!(lines[1].content, "---hello");
        assert_eq!(lines[2].content, "regular");
    }

    #[test]
    fn is_diff_header_distinguishes_headers_from_content() {
        assert!(is_diff_header("+++ b/src/lib.rs"));
        assert!(is_diff_header("--- a/src/lib.rs"));
        assert!(is_diff_header("+++ /dev/null"));
        assert!(is_diff_header("--- /dev/null"));
        // Content lines whose body starts with the marker characters but no
        // separating space — must NOT be treated as headers.
        assert!(!is_diff_header("+++hi"));
        assert!(!is_diff_header("---"));
        assert!(!is_diff_header("+++"));
    }

    #[test]
    fn window_overlap_computes_intersection() {
        assert_eq!(window_overlap([1, 5], [3, 7]), 3); // [3,4,5]
        assert_eq!(window_overlap([1, 3], [4, 6]), 0);
        assert_eq!(window_overlap([1, 1], [1, 1]), 1);
    }

    #[test]
    fn normalize_collapses_whitespace_and_quotes() {
        assert_eq!(normalize("  alpha   beta  "), " alpha beta");
        assert_eq!(normalize("\u{201C}hi\u{201D}"), "\"hi\"");
    }
}
