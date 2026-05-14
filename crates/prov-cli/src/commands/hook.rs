//! Hook-event dispatch.
//!
//! Called by agent harness hooks (`UserPromptSubmit`, `PostToolUse`, `Stop`,
//! `SessionStart`) and by git hooks (`post-commit`, `post-rewrite`, `pre-push`).
//!
//! **Defensive contract.** All hook subcommands always exit `0` — even on
//! internal error — and log to `<git-dir>/prov-staging/log`. A capture failure
//! must never block the agent loop nor the user's commit. The few branches
//! that intentionally block (e.g., U8's pre-push gate when an unredacted
//! secret is detected) live in dedicated handlers, not here.
//!
//! Each handler reads its hook payload from stdin (the harness hook
//! contract) and runs `Redactor::redact` over any prompt-or-summary text
//! before staging. Even local-only staging is scrubbed: a future opt-in
//! `prov push` should never find raw secrets in the staging tree.
//!
//! The Claude parser operates on the documented `tool_input` envelope
//! (Edit/Write/MultiEdit shapes). The Codex parser starts with `apply_patch`
//! command payloads, matching Codex's documented file-edit hook surface.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde::Deserialize;

use prov_core::git::{Git, GitError};
use prov_core::privacy::is_prov_private;
use prov_core::redactor::provignore::{ProvIgnore, ProvIgnoreError};
use prov_core::redactor::Redactor;
use prov_core::schema::{DerivedFrom, Edit, Note};
use prov_core::session::SessionId;
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::staging::{EditRecord, SessionMeta, Staging, StagingError, TurnRecord};
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

use super::common::CACHE_FILENAME;
use prov_core::time::now_iso8601;

const TOOL_CLAUDE_CODE: &str = "claude-code";
const TOOL_CODEX: &str = "codex";

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
    /// Claude Code hook event with explicit adapter selection.
    Claude {
        #[command(subcommand)]
        event: AgentEvent,
    },
    /// Codex hook event with explicit adapter selection.
    Codex {
        #[command(subcommand)]
        event: AgentEvent,
    },
    /// Git: `post-commit` — flush staged edits into a note attached to HEAD.
    PostCommit,
    /// Git: `post-rewrite` — reattach notes after amend/rebase/squash.
    PostRewrite {
        /// `amend` or `rebase` — git passes this as the first arg.
        kind: String,
    },
    /// Git: `pre-push` — scan notes refs for unredacted secrets before push. Owned by U8.
    PrePush,
}

#[derive(Subcommand, Debug, Clone, Copy)]
pub enum AgentEvent {
    /// `UserPromptSubmit` — stage prompt + session metadata.
    UserPromptSubmit,
    /// `PostToolUse` — stage file edits.
    PostToolUse,
    /// `Stop` — mark the current turn complete.
    Stop,
    /// `SessionStart` — capture model name for this session.
    SessionStart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentHarness {
    Claude,
    Codex,
}

impl AgentHarness {
    fn tool(self) -> &'static str {
        match self {
            Self::Claude => TOOL_CLAUDE_CODE,
            Self::Codex => TOOL_CODEX,
        }
    }
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
        Event::UserPromptSubmit => handle_user_prompt_submit(&staging, AgentHarness::Claude, &git),
        Event::PostToolUse => {
            handle_post_tool_use(&staging, AgentHarness::Claude, Some(git.work_tree()))
        }
        Event::Stop => handle_stop(&staging, AgentHarness::Claude),
        Event::SessionStart => handle_session_start(&staging, AgentHarness::Claude),
        Event::Claude { event } => handle_agent_event(&staging, AgentHarness::Claude, event, &git),
        Event::Codex { event } => handle_agent_event(&staging, AgentHarness::Codex, event, &git),
        Event::PostCommit => handle_post_commit(&git, &staging),
        Event::PostRewrite { kind } => handle_post_rewrite(&git, &staging, &kind),
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

fn handle_agent_event(
    staging: &Staging,
    harness: AgentHarness,
    event: AgentEvent,
    git: &Git,
) -> Result<(), HandlerError> {
    match event {
        AgentEvent::UserPromptSubmit => handle_user_prompt_submit(staging, harness, git),
        AgentEvent::PostToolUse => handle_post_tool_use(staging, harness, Some(git.work_tree())),
        AgentEvent::Stop => handle_stop(staging, harness),
        AgentEvent::SessionStart => handle_session_start(staging, harness),
    }
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
    turn_id: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
}

fn handle_user_prompt_submit(
    staging: &Staging,
    _harness: AgentHarness,
    git: &Git,
) -> Result<(), HandlerError> {
    let payload: UserPromptSubmitPayload = read_stdin_json()?;
    let raw_session = payload
        .session_id
        .or_else(|| payload.turn_id.clone())
        .unwrap_or_default();
    let prompt = payload.prompt.unwrap_or_default();
    let Ok(sid) = SessionId::parse(raw_session) else {
        return Ok(());
    };

    // First/last-line `# prov:private` opt-out (case-insensitive). A
    // `# prov:private` inside a code-block paste does not flip the routing.
    // Predicate lives in prov_core::privacy so `prov backfill` honors the
    // same routing rule when reconstructing prompts from transcripts.
    let private = is_prov_private(&prompt);

    // Redact even staged content. The redactor is the primary defense; pre-push
    // (U8) is the second line.
    let redactor = match redactor_for_repo(git.work_tree()) {
        Ok(redactor) => redactor,
        Err(e) => {
            let _ = staging.append_log(&format!("{}: .provignore load failed: {e}", now_iso8601()));
            Redactor::new()
        }
    };
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

// =================================================================
// PostToolUse
// =================================================================

#[derive(Debug, Deserialize)]
struct PostToolUsePayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_input: serde_json::Value,
    #[serde(default)]
    tool_response: serde_json::Value,
    /// Path to the session transcript JSONL. The PostToolUse hook payload
    /// always carries this; we read the most recent assistant entry from it
    /// to capture the *current* model rather than the (possibly stale)
    /// SessionStart model — `/model` does not re-fire SessionStart.
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

fn handle_post_tool_use(
    staging: &Staging,
    harness: AgentHarness,
    work_tree: Option<&Path>,
) -> Result<(), HandlerError> {
    let payload: PostToolUsePayload = read_stdin_json()?;
    let raw_session = payload
        .session_id
        .or_else(|| payload.turn_id.clone())
        .unwrap_or_default();
    let Ok(sid) = SessionId::parse(raw_session) else {
        return Ok(());
    };
    let tool_name = payload.tool_name.unwrap_or_default();
    if harness == AgentHarness::Claude
        && !matches!(tool_name.as_str(), "Edit" | "Write" | "MultiEdit")
    {
        return Ok(());
    }
    if harness == AgentHarness::Codex && tool_name != "apply_patch" {
        return Ok(());
    }

    // Public/private routing: use the most-recent turn's `private` flag.
    let private = current_turn_is_private(staging, &sid);
    let turn_index = staging
        .count_turns(&sid, private)
        .unwrap_or(0)
        .saturating_sub(1);

    // Read the model from the transcript so we capture the model that
    // produced *this* edit, not whatever was set when SessionStart fired.
    // `None` is fine — flush_visibility falls back to SessionMeta.model.
    let model = payload
        .transcript_path
        .as_deref()
        .and_then(read_latest_assistant_model)
        .or(payload.model);

    let edits = decompose_tool_use(
        &tool_name,
        harness,
        &payload.tool_input,
        &payload.tool_response,
        &sid,
        turn_index,
        payload.tool_use_id.as_deref(),
        model.as_deref(),
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
#[allow(clippy::too_many_arguments)]
fn decompose_tool_use(
    tool_name: &str,
    harness: AgentHarness,
    tool_input: &serde_json::Value,
    _tool_response: &serde_json::Value,
    sid: &SessionId,
    turn_index: u32,
    tool_use_id: Option<&str>,
    model: Option<&str>,
    work_tree: Option<&Path>,
) -> Vec<EditRecord> {
    let timestamp = now_iso8601();
    match (harness, tool_name) {
        (AgentHarness::Claude, "Edit") => {
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
                harness.tool(),
                tool_use_id,
                model,
                file,
                old,
                new,
                &timestamp,
                work_tree,
            )]
        }
        (AgentHarness::Claude, "Write") => {
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
                harness.tool(),
                tool_use_id,
                model,
                file,
                "",
                content,
                &timestamp,
                work_tree,
            )]
        }
        (AgentHarness::Claude, "MultiEdit") => {
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
                        harness.tool(),
                        tool_use_id,
                        model,
                        file,
                        old,
                        new,
                        &timestamp,
                        work_tree,
                    )
                })
                .collect()
        }
        (AgentHarness::Codex, "apply_patch") => decompose_apply_patch_tool_use(
            tool_input,
            sid,
            turn_index,
            tool_use_id,
            model,
            &timestamp,
            work_tree,
        ),
        _ => Vec::new(),
    }
}

fn decompose_apply_patch_tool_use(
    tool_input: &serde_json::Value,
    sid: &SessionId,
    turn_index: u32,
    tool_use_id: Option<&str>,
    model: Option<&str>,
    timestamp: &str,
    work_tree: Option<&Path>,
) -> Vec<EditRecord> {
    let Some(command) = tool_input.get("command").and_then(|v| v.as_str()) else {
        return Vec::new();
    };
    parse_apply_patch_command(command)
        .into_iter()
        .map(|patch| {
            record_for(
                sid,
                turn_index,
                "apply_patch",
                TOOL_CODEX,
                tool_use_id,
                model,
                &patch.file,
                "",
                &patch.added_lines.join("\n"),
                timestamp,
                work_tree,
            )
        })
        .collect()
}

struct ParsedPatchFile {
    file: String,
    added_lines: Vec<String>,
}

fn parse_apply_patch_command(command: &str) -> Vec<ParsedPatchFile> {
    let mut files = Vec::new();
    let mut current: Option<ParsedPatchFile> = None;
    for line in command.lines() {
        if let Some(file) = line
            .strip_prefix("*** Update File: ")
            .or_else(|| line.strip_prefix("*** Add File: "))
        {
            if let Some(done) = current.take() {
                if !done.added_lines.is_empty() {
                    files.push(done);
                }
            }
            current = Some(ParsedPatchFile {
                file: file.trim().to_string(),
                added_lines: Vec::new(),
            });
            continue;
        }
        if line.starts_with("*** ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            if let Some(active) = current.as_mut() {
                active.added_lines.push(rest.to_string());
            }
        }
    }
    if let Some(done) = current {
        if !done.added_lines.is_empty() {
            files.push(done);
        }
    }
    files
}

#[allow(clippy::too_many_arguments)]
fn record_for(
    sid: &SessionId,
    turn_index: u32,
    tool_name: &str,
    tool: &str,
    tool_use_id: Option<&str>,
    model: Option<&str>,
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
        tool: tool.to_string(),
        tool_use_id: tool_use_id.map(str::to_string),
        file: relativize_for_storage(file, work_tree),
        line_range,
        before: before.to_string(),
        after: after.to_string(),
        content_hashes,
        model: model.map(str::to_string),
        timestamp: timestamp.to_string(),
    }
}

/// Read `transcript_path` and return the `message.model` field of the most
/// recent `type:"assistant"` entry. Returns `None` for any failure (missing
/// file, malformed JSON, no assistant entries, no model field) — callers fall
/// back to `SessionMeta.model`. A capture failure must never block the agent.
///
/// Hard-cap the bytes we read: real transcripts grow into the megabytes, but
/// we only need the tail. Read the full file and walk lines from the end —
/// JSONL is line-delimited, so reading bottom-up is the natural order.
fn read_latest_assistant_model(path: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    /// Cap on transcript bytes scanned. A long-running session can produce
    /// tens of MB of transcript; reading all of it on every PostToolUse would
    /// be wasteful when the only data we need is in the last assistant entry
    /// (typically the final few KB).
    const MAX_TAIL_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB

    let metadata = std::fs::metadata(path).ok()?;
    let len = metadata.len();
    let read_from = len.saturating_sub(MAX_TAIL_BYTES);

    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(read_from)).ok()?;
    let mut buf = Vec::with_capacity(usize::try_from(len - read_from).unwrap_or(0));
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);

    // If we tail-read past a partial first line, drop it — JSONL parses fail
    // on partial lines anyway, but discarding cleanly keeps the loop tidy.
    let mut iter = text.lines();
    if read_from > 0 {
        iter.next();
    }
    let lines: Vec<&str> = iter.collect();

    for line in lines.iter().rev() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let model = obj
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(|v| v.as_str())?;
        if model.is_empty() {
            continue;
        }
        return Some(model.to_string());
    }
    None
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
    #[serde(default)]
    turn_id: Option<String>,
}

fn handle_stop(staging: &Staging, _harness: AgentHarness) -> Result<(), HandlerError> {
    let payload: StopPayload = read_stdin_json()?;
    let Ok(sid) = SessionId::parse(
        payload
            .session_id
            .or_else(|| payload.turn_id.clone())
            .unwrap_or_default(),
    ) else {
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
    turn_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

fn handle_session_start(staging: &Staging, _harness: AgentHarness) -> Result<(), HandlerError> {
    let payload: SessionStartPayload = read_stdin_json()?;
    let Ok(sid) = SessionId::parse(
        payload
            .session_id
            .or_else(|| payload.turn_id.clone())
            .unwrap_or_default(),
    ) else {
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
            // Per-edit model (read from transcript at capture time) wins over
            // SessionMeta.model. SessionStart only fires once per session, so
            // a `/model` switch mid-session would otherwise mis-attribute every
            // later turn. Legacy records (no model field) fall back.
            let model = er
                .model
                .clone()
                .unwrap_or_else(|| session_meta.model.clone());
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
                model,
                tool: er.tool.clone(),
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
// post-rewrite (git hook)
// =================================================================

/// Read the post-rewrite stdin protocol: each line is `<old-sha> <new-sha>`.
/// Lines that do not parse to two SHAs are skipped — defensive: a Prov bug
/// here must not break the surrounding rebase.
fn read_post_rewrite_pairs() -> Result<Vec<(String, String)>, HandlerError> {
    const MAX_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

    let mut buf = String::new();
    let bytes_read = io::stdin()
        .take(MAX_PAYLOAD_BYTES + 1)
        .read_to_string(&mut buf)
        .map_err(|e| HandlerError::Stdin(e.to_string()))?;
    if u64::try_from(bytes_read).unwrap_or(u64::MAX) > MAX_PAYLOAD_BYTES {
        return Err(HandlerError::Stdin(format!(
            "post-rewrite payload exceeded {MAX_PAYLOAD_BYTES} bytes"
        )));
    }

    let mut out = Vec::new();
    for raw in buf.lines() {
        let mut parts = raw.split_whitespace();
        let (Some(old), Some(new)) = (parts.next(), parts.next()) else {
            continue;
        };
        if !is_full_hex_sha(old) || !is_full_hex_sha(new) {
            continue;
        }
        out.push((old.to_string(), new.to_string()));
    }
    Ok(out)
}

/// Post-rewrite handler. Migrates notes from old SHAs to new SHAs after
/// `git commit --amend` or `git rebase` (interactive or otherwise).
///
/// Strategy by mapping shape:
/// - **1:1** (amend, simple rebase): copy the old SHA's note verbatim onto
///   the new SHA, then delete the old.
/// - **N:1** (squash): merge the N old notes' `edits[]` into a single note,
///   deduplicating by `(conversation_id, turn_index, tool_use_id)` and
///   sorting by `timestamp`, then delete each old.
///
/// `notes.rewrite.amend` and `notes.rewrite.rebase` are set to `false` by
/// `prov install` so git never auto-concatenates the JSON blobs (which would
/// produce invalid `{...}{...}` content). This handler is the sole writer.
///
/// Both the public `refs/notes/prompts` and private `refs/notes/prompts-private`
/// refs are migrated, so a private note attached to a rewritten commit follows
/// the rewrite without leaking out of the private ref.
///
/// `kind` is `"amend"` or `"rebase"` per githooks(5); v1 ignores the
/// distinction — the squash-vs-1:1 decision falls out of the stdin pairs.
fn handle_post_rewrite(git: &Git, staging: &Staging, kind: &str) -> Result<(), HandlerError> {
    let pairs = read_post_rewrite_pairs()?;
    if pairs.is_empty() {
        return Ok(());
    }

    // Group old SHAs by their target new SHA so we can detect N:1 squashes.
    let mut by_new: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (old, new) in &pairs {
        by_new.entry(new.clone()).or_default().push(old.clone());
    }

    for (new_sha, old_shas) in by_new {
        for ref_name in [NOTES_REF_PUBLIC, NOTES_REF_PRIVATE] {
            if let Err(e) = migrate_one(git, staging, &new_sha, &old_shas, ref_name) {
                staging
                    .append_log(&format!(
                        "{}: post-rewrite ({kind}) ref={ref_name} new={new_sha} failed: {e}",
                        now_iso8601()
                    ))
                    .ok();
            }
        }
    }

    // Cache reflects whatever the notes ref now contains. Drop the cache
    // entries for every touched commit and re-stamp the public ref so the
    // resolver doesn't fire its drift-detection reindex on the next read.
    // The new-SHA notes lazy-load via the existing reindex-on-drift path.
    let touched: std::collections::BTreeSet<&str> = pairs
        .iter()
        .flat_map(|(o, n)| [o.as_str(), n.as_str()])
        .collect();
    super::common::invalidate_cache_per_sha(git, touched);

    Ok(())
}

/// Migrate notes for a single `(new_sha, [old_shas])` group on one notes ref.
/// Reads each present old note; for 1:1 writes the lone note verbatim, for
/// N:1 merges edits arrays. Removes old notes only after the new write
/// succeeds, so a crash mid-handler leaves the source notes intact.
fn migrate_one(
    git: &Git,
    staging: &Staging,
    new_sha: &str,
    old_shas: &[String],
    ref_name: &str,
) -> Result<(), anyhow::Error> {
    let store = NotesStore::new(git.clone(), ref_name);

    // Collect any notes that actually exist on the old SHAs. Old SHAs without
    // a note simply contribute nothing (the squashed commit may include some
    // commits that had no AI provenance).
    let mut sources: Vec<(String, Note)> = Vec::new();
    for old in old_shas {
        if let Some(note) = store.read(old).context("read old note")? {
            sources.push((old.clone(), note));
        }
    }
    if sources.is_empty() {
        return Ok(());
    }

    // Don't clobber an existing note on the new SHA — the post-commit handler
    // may already have written one (e.g., rebase that re-applies a commit and
    // re-runs hooks). Merge those edits in too so we don't drop history.
    let existing_on_new = store.read(new_sha).context("read new note")?;

    let merged = if sources.len() == 1 && existing_on_new.is_none() {
        // 1:1 fast path — no merge needed.
        sources.into_iter().next().map(|(_, n)| n).unwrap()
    } else {
        let mut all_edits: Vec<Edit> = Vec::new();
        if let Some(n) = existing_on_new {
            all_edits.extend(n.edits);
        }
        for (_, n) in sources {
            all_edits.extend(n.edits);
        }
        Note::new(dedupe_and_sort_edits(all_edits))
    };

    store.write(new_sha, &merged).context("write new note")?;

    // Remove old notes after the write succeeds. If a remove fails, the new
    // note is already in place — the orphan cleanup is best-effort. Log
    // failures so silent orphan accumulation surfaces in the staging log
    // rather than only via `prov gc` later.
    for old in old_shas {
        if old == new_sha {
            // Defensive: a no-op rewrite (some `git rebase` paths emit
            // identical pairs) would otherwise delete the note we just wrote.
            continue;
        }
        if let Err(e) = store.remove(old) {
            staging
                .append_log(&format!(
                    "{}: post-rewrite remove old note failed (ref={ref_name} old={old}): {e}",
                    now_iso8601()
                ))
                .ok();
        }
    }
    Ok(())
}

/// Deduplicate edits and sort by `timestamp` ascending. Edits that share the
/// dedupe key are collapsed keeping the entry with the latest `timestamp` so
/// a later-captured version wins over an earlier one (matters when the same
/// edit was re-staged).
///
/// Dedupe key:
/// - When `tool_use_id` is `Some(_)`: `(conversation_id, turn_index, tool_use_id)`.
/// - When `tool_use_id` is `None`: fall back to
///   `(conversation_id, turn_index, file, line_range)` so distinct file regions
///   in the same turn don't collapse. Without this fallback, a MultiEdit whose
///   inner edits don't surface a tool_use_id (or a `prov backfill` note that
///   has none) would silently lose all but one edit on squash.
fn dedupe_and_sort_edits(edits: Vec<Edit>) -> Vec<Edit> {
    use std::collections::BTreeMap;
    // `Either` shape inlined as a tuple of (primary-key, fallback-discriminator).
    // The fallback string is empty when `tool_use_id.is_some()` so the two
    // shapes can't collide in the same map.
    type Key = (String, u32, Option<String>, String);
    let mut by_key: BTreeMap<Key, Edit> = BTreeMap::new();
    for e in edits {
        let fallback = if e.tool_use_id.is_none() {
            format!("{}@{}-{}", e.file, e.line_range[0], e.line_range[1])
        } else {
            String::new()
        };
        let key: Key = (
            e.conversation_id.clone(),
            e.turn_index,
            e.tool_use_id.clone(),
            fallback,
        );
        match by_key.get(&key) {
            Some(existing) if existing.timestamp >= e.timestamp => {}
            _ => {
                by_key.insert(key, e);
            }
        }
    }
    let mut out: Vec<Edit> = by_key.into_values().collect();
    out.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    out
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
    let redactor = match redactor_for_repo(git.work_tree()) {
        Ok(redactor) => redactor,
        Err(e) => {
            return Ok(PrePushOutcome::Block(vec![format!(
                "prov pre-push: .provignore could not be loaded: {e}"
            )]));
        }
    };

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

fn redactor_for_repo(work_tree: &Path) -> Result<Redactor, ProvIgnoreError> {
    let provignore = ProvIgnore::from_path(work_tree.join(".provignore"))?;
    Ok(Redactor::new().with_provignore(provignore))
}

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

    // Behavioral coverage of `is_prov_private` lives in
    // `prov_core::privacy`'s unit tests. The hook's job here is only to
    // route on the predicate's verdict — see `private` at line 165.

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
            AgentHarness::Claude,
            &input,
            &serde_json::Value::Null,
            &sid,
            0,
            Some("toolu_1"),
            None,
            None,
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].tool_name, "Edit");
        assert_eq!(recs[0].file, "src/lib.rs");
        assert_eq!(recs[0].after, "new");
    }

    #[test]
    fn decompose_carries_model_into_record() {
        // The model captured at PostToolUse time (read from the transcript)
        // must land on the EditRecord so the post-commit flush uses it
        // verbatim instead of falling back to SessionMeta.model.
        let sid = SessionId::parse("sess_t").unwrap();
        let input = serde_json::json!({
            "file_path": "src/lib.rs",
            "old_string": "old",
            "new_string": "new",
        });
        let recs = decompose_tool_use(
            "Edit",
            AgentHarness::Claude,
            &input,
            &serde_json::Value::Null,
            &sid,
            0,
            Some("toolu_1"),
            Some("claude-sonnet-4-6"),
            None,
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].model.as_deref(), Some("claude-sonnet-4-6"));
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
            AgentHarness::Claude,
            &input,
            &serde_json::Value::Null,
            &sid,
            0,
            Some("toolu_1"),
            None,
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
            AgentHarness::Claude,
            &input,
            &serde_json::Value::Null,
            &sid,
            0,
            None,
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
            AgentHarness::Claude,
            &input,
            &serde_json::Value::Null,
            &sid,
            0,
            Some("toolu_1"),
            Some("claude-sonnet-4-6"),
            None,
        );
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].after, "1");
        assert_eq!(recs[2].after, "3");
        // Same model is stamped on every inner-edit record so a per-MultiEdit
        // model never gets lost halfway through the loop.
        assert!(recs
            .iter()
            .all(|r| r.model.as_deref() == Some("claude-sonnet-4-6")));
    }

    #[test]
    fn decompose_unknown_tool_is_empty() {
        let sid = SessionId::parse("sess_t").unwrap();
        let recs = decompose_tool_use(
            "SomethingElse",
            AgentHarness::Claude,
            &serde_json::Value::Null,
            &serde_json::Value::Null,
            &sid,
            0,
            None,
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
            tool: TOOL_CLAUDE_CODE.into(),
            tool_use_id: None,
            file: "/tmp/repo/src/lib.rs".into(),
            line_range: [1, 2],
            before: String::new(),
            after: "alpha\nbeta\n".into(),
            content_hashes: vec![],
            model: None,
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

    fn write_transcript(lines: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        let mut body = String::new();
        for l in lines {
            body.push_str(l);
            body.push('\n');
        }
        std::fs::write(&path, body).expect("write transcript");
        (dir, path)
    }

    #[test]
    fn read_latest_assistant_model_returns_most_recent_model() {
        // The transcript may contain mixed `user`, `system`, and multiple
        // `assistant` entries; we want the model from the LAST assistant
        // entry (the one that just produced the tool call we're staging).
        let (_dir, path) = write_transcript(&[
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"user","message":{}}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-7[1m]","content":[]}}"#,
            r#"{"type":"user","message":{}}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6","content":[]}}"#,
        ]);
        let model = read_latest_assistant_model(path.to_str().unwrap());
        assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn read_latest_assistant_model_handles_missing_file() {
        // Defensive: a hook payload that points at a path that doesn't exist
        // (or a file we can't read) must return None, not panic — the caller
        // falls back to SessionMeta.model on None.
        assert_eq!(read_latest_assistant_model("/no/such/path.jsonl"), None);
    }

    #[test]
    fn read_latest_assistant_model_skips_non_assistant_entries() {
        let (_dir, path) = write_transcript(&[
            r#"{"type":"user","message":{"model":"not-this-one"}}"#,
            r#"{"type":"system","subtype":"compact"}"#,
        ]);
        assert_eq!(read_latest_assistant_model(path.to_str().unwrap()), None);
    }

    #[test]
    fn read_latest_assistant_model_skips_malformed_lines() {
        // A truncated trailing line (or any non-JSON garbage) must not abort
        // the scan; we walk past it to the most recent valid assistant entry.
        let (_dir, path) = write_transcript(&[
            r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6"}}"#,
            "not valid json",
        ]);
        assert_eq!(
            read_latest_assistant_model(path.to_str().unwrap()).as_deref(),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn read_latest_assistant_model_treats_empty_string_as_missing() {
        let (_dir, path) = write_transcript(&[
            r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6"}}"#,
            r#"{"type":"assistant","message":{"model":""}}"#,
        ]);
        // Skip the empty-model entry and fall back to the prior valid one.
        assert_eq!(
            read_latest_assistant_model(path.to_str().unwrap()).as_deref(),
            Some("claude-sonnet-4-6")
        );
    }
}
