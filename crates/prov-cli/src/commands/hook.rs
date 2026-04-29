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
//! Empirical risk note (per the v1 plan, "Open Questions → Deferred to
//! Implementation"): the exact shape of `tool_response.structuredPatch` is not
//! formally documented. The parsers below operate on the documented
//! `tool_input` envelope (Edit/Write/MultiEdit shapes per the Claude Code
//! tool docs). A live-session verification step is tracked as a follow-up
//! after U3 lands.

use std::io::{self, Read};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use serde::Deserialize;

use prov_core::git::{Git, GitError};
use prov_core::redactor::Redactor;
use prov_core::schema::{DerivedFrom, Edit, Note};
use prov_core::session::SessionId;
use prov_core::storage::notes::NotesStore;
use prov_core::storage::staging::{EditRecord, SessionMeta, Staging, StagingError, TurnRecord};
use prov_core::storage::NOTES_REF_PUBLIC;

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
/// dispatch shape; this handler must never propagate errors out.
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
    let event_label = format!("{:?}", args.event);

    let result = match args.event {
        Event::UserPromptSubmit => handle_user_prompt_submit(&staging),
        Event::PostToolUse => handle_post_tool_use(&staging),
        Event::Stop => handle_stop(&staging),
        Event::SessionStart => handle_session_start(&staging),
        Event::PostCommit => handle_post_commit(&git, &staging),
        // U9 owns post-rewrite, U8 owns pre-push. Land here as no-ops so the
        // git hook scripts can wire the command without breaking; later units
        // fill in real behaviour.
        Event::PostRewrite { .. } | Event::PrePush => Ok(()),
    };

    if let Err(e) = result {
        let _ = staging.append_log(&format!(
            "{}: hook {event_label} failed: {e}",
            now_iso8601()
        ));
    }
    Ok(())
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

fn handle_post_tool_use(staging: &Staging) -> Result<(), HandlerError> {
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

    // TODO(U3-empirical): verify against a live Claude Code session that
    // `tool_use_id` is consistently present and that the tool_input shapes
    // below match live payloads. The unit tests use synthesized fixtures.
    let edits = decompose_tool_use(
        &tool_name,
        &payload.tool_input,
        &payload.tool_response,
        &sid,
        turn_index,
        payload.tool_use_id.as_deref(),
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
    let public = staging.read_turns(sid, false).unwrap_or_default();
    let private = staging.read_turns(sid, true).unwrap_or_default();
    let last_public_started = public.last().map(|t| t.started_at.clone());
    let last_private_started = private.last().map(|t| t.started_at.clone());
    match (last_public_started, last_private_started) {
        (Some(p), Some(pr)) => pr > p,
        (None, Some(_)) => true,
        _ => false,
    }
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
        file: file.to_string(),
        line_range,
        before: before.to_string(),
        after: after.to_string(),
        content_hashes,
        timestamp: timestamp.to_string(),
    }
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
    let mut matched_edits: Vec<Edit> = Vec::new();
    let mut matched_keys: Vec<(SessionId, MatchedKey)> = Vec::new();

    for sid in &sessions {
        let session_meta = staging
            .read_session_meta(sid)
            .ok()
            .flatten()
            .unwrap_or_else(|| SessionMeta {
                session_id: sid.as_str().to_string(),
                model: "unknown".into(),
                started_at: String::new(),
            });
        let turns = staging.read_turns(sid, false).unwrap_or_default();
        let edits = staging.read_edits(sid, false).unwrap_or_default();

        for er in &edits {
            let Some(matched) = match_edit_to_diff(er, &added_by_file) else {
                continue;
            };
            let prompt = turns
                .iter()
                .find(|t| t.turn_index == er.turn_index)
                .map(|t| t.prompt.clone())
                .unwrap_or_default();

            matched_edits.push(Edit {
                file: er.file.clone(),
                line_range: matched.line_range,
                content_hashes: er.content_hashes.clone(),
                original_blob_sha: String::new(),
                prompt,
                conversation_id: er.session_id.clone(),
                turn_index: er.turn_index,
                tool_use_id: er.tool_use_id.clone(),
                preceding_turns_summary: String::new(),
                model: session_meta.model.clone(),
                tool: "claude-code".into(),
                timestamp: er.timestamp.clone(),
                derived_from: None,
            });
            matched_keys.push((sid.clone(), matched.key));
        }
    }

    if !matched_edits.is_empty() {
        let mut note = Note::new(matched_edits);
        // Cherry-pick: stamp every matched edit with `derived_from: Rewrite`
        // pointing back to the source commit. v1 keeps this coarse; U9 owns
        // the precise per-edit `source_edit` index pinning.
        if let Some(source) = &cherry_pick_source {
            for edit in &mut note.edits {
                edit.derived_from = Some(DerivedFrom::Rewrite {
                    source_commit: source.clone(),
                    source_edit: 0,
                });
            }
        }
        let store = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
        if let Err(e) = store.write(&head, &note) {
            staging
                .append_log(&format!("{}: notes.write failed: {e}", now_iso8601()))
                .ok();
            return Ok(());
        }
    }

    // Cleanup: if every staged edit in a session matched, remove the session
    // dir. Partial-match cleanup (rewriting edits.jsonl) is U9-territory.
    for sid in &sessions {
        let still = staging.read_edits(sid, false).unwrap_or_default();
        let matched_in_session = matched_keys.iter().filter(|(s, _)| s == sid).count();
        if !still.is_empty() && matched_in_session == still.len() {
            staging.remove_session(sid).ok();
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MatchedKey {
    file: String,
    line_range: [u32; 2],
}

#[derive(Debug)]
struct Match {
    line_range: [u32; 2],
    key: MatchedKey,
}

fn match_edit_to_diff(
    er: &EditRecord,
    added_by_file: &std::collections::HashMap<String, Vec<AddedLine>>,
) -> Option<Match> {
    let added = added_by_file.get(&er.file)?;
    if added.is_empty() {
        return None;
    }

    // Strategy a: exact match — every captured `after` line appears verbatim
    // as a contiguous run in the diff's added lines.
    let added_content: Vec<String> = added.iter().map(|l| l.content.clone()).collect();
    let after_lines: Vec<&str> = er.after.split('\n').collect();
    if let Some(range) = exact_window_match_str(&after_lines, &added_content, added) {
        return Some(Match {
            line_range: range,
            key: MatchedKey {
                file: er.file.clone(),
                line_range: range,
            },
        });
    }

    // Strategy b: normalized — strip trailing whitespace, collapse internal
    // runs, normalize ASCII quote style. Tolerates prettier/black/rustfmt
    // running between PostToolUse and the commit.
    let norm_after: Vec<String> = after_lines.iter().map(|l| normalize(l)).collect();
    let norm_added: Vec<String> = added_content.iter().map(|l| normalize(l)).collect();
    if let Some(range) = exact_window_match_norm(&norm_after, &norm_added, added) {
        return Some(Match {
            line_range: range,
            key: MatchedKey {
                file: er.file.clone(),
                line_range: range,
            },
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
                line_range: window,
                key: MatchedKey {
                    file: er.file.clone(),
                    line_range: window,
                },
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
        if line.starts_with("+++") || line.starts_with("---") {
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

fn read_cherry_pick_head(git: &Git) -> Option<String> {
    let path = git.git_dir().join("CHERRY_PICK_HEAD");
    let s = std::fs::read_to_string(path).ok()?;
    Some(s.trim().to_string())
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
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| HandlerError::Stdin(e.to_string()))?;
    if buf.trim().is_empty() {
        // Treat empty stdin as "{}" so handlers fall through to their default
        // (which is typically "do nothing"). Lets the git-hook wrapper invoke
        // these without a payload during scaffolding.
        buf = "{}".to_string();
    }
    serde_json::from_str(&buf).map_err(HandlerError::from)
}

fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (year, month, day, hour, minute, second) = epoch_to_civil(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's civil-from-days. Pure integer arithmetic. Variable names
/// (`z`, `era`, `doe`, `yoe`, `doy`, `mp`) follow the canonical paper so the
/// algorithm is recognisable; the names are intentionally short and similar.
#[allow(clippy::similar_names, clippy::many_single_char_names)]
fn epoch_to_civil(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let day_secs = 86_400_u64;
    let z = i64::try_from(secs / day_secs).unwrap_or(0) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i32::try_from(yoe).unwrap_or(0) + i32::try_from(era).unwrap_or(0) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let month = u32::try_from(m).unwrap_or(0);
    let day = u32::try_from(d).unwrap_or(0);
    let day_secs_offset = secs % day_secs;
    let hour = u32::try_from(day_secs_offset / 3600).unwrap_or(0);
    let minute = u32::try_from((day_secs_offset % 3600) / 60).unwrap_or(0);
    let second = u32::try_from(day_secs_offset % 60).unwrap_or(0);
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_format_is_parseable() {
        let s = now_iso8601();
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), 20);
    }

    #[test]
    fn epoch_to_civil_handles_known_values() {
        // 1970-01-01T00:00:00Z
        assert_eq!(epoch_to_civil(0), (1970, 1, 1, 0, 0, 0));
        // 2024-01-01T00:00:00Z = 1_704_067_200
        assert_eq!(epoch_to_civil(1_704_067_200), (2024, 1, 1, 0, 0, 0));
        // 2026-04-28T12:34:56Z = 1_777_379_696
        assert_eq!(epoch_to_civil(1_777_379_696), (2026, 4, 28, 12, 34, 56));
    }

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
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].tool_name, "Edit");
        assert_eq!(recs[0].file, "src/lib.rs");
        assert_eq!(recs[0].after, "new");
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
        );
        assert!(recs.is_empty());
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
