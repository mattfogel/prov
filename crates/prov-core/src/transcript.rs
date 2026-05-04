//! Parser for Claude Code session transcript JSONL files.
//!
//! Used by `prov backfill` to reconstruct sessions from on-disk transcripts that
//! were never seen by the live capture pipeline. The format is undocumented;
//! the parser is best-effort and tolerant of malformed lines (logs and skips
//! rather than failing the whole file).
//!
//! Shape (empirical, 2026-05; Claude Code 2.1.x):
//! - One JSON object per line. Top-level `type` discriminates events.
//! - `type: "user"` carries either a string `message.content` (the user's typed
//!   prompt or a slash-command stub) or an array of content blocks. Tool-result
//!   echoes (`type: "tool_result"` blocks) and meta entries (`isMeta: true`)
//!   are filtered out — they are framework chatter, not user intent.
//! - Each user prompt has a stable `promptId`; multiple consecutive user
//!   entries can share one. The first non-meta, non-tool-result entry per
//!   `promptId` defines the turn (and its timestamp).
//! - `type: "assistant"` carries `message.content` as a content-block array.
//!   `tool_use` blocks for `Edit`, `Write`, or `MultiEdit` (matching the
//!   capture pipeline's tool gate in U3) become edits attributed to the most
//!   recent user turn at parse time.
//! - Other top-level types (`permission-mode`, `file-history-snapshot`,
//!   `summary`, `system`, etc.) are ignored.

use std::path::Path;

use serde_json::Value;

/// One Claude Code session reconstructed from a transcript file.
#[derive(Debug, Clone)]
pub struct ParsedSession {
    /// `sessionId` carried on every entry (also encoded in the filename).
    pub session_id: String,
    /// ISO-8601 timestamp of the first event with a `timestamp` field.
    pub started_at: Option<String>,
    /// ISO-8601 timestamp of the last event with a `timestamp` field.
    pub ended_at: Option<String>,
    /// Most recently seen `message.model` across assistant entries. Pinned at
    /// parse time so a `/model` switch surfaces the *latest* model rather than
    /// the SessionStart-time one (matches the live-capture model-pinning rule
    /// in `hook::handle_post_tool_use`).
    pub model: Option<String>,
    /// First non-empty `cwd` observed. Helpful for sanity-checking that the
    /// transcript belongs to the project we're backfilling.
    pub cwd: Option<String>,
    /// User turns in the order they appeared.
    pub turns: Vec<ParsedTurn>,
    /// Edits attributed to the most-recent user turn at parse time.
    pub edits: Vec<ParsedEdit>,
}

/// One user turn extracted from the transcript.
#[derive(Debug, Clone)]
pub struct ParsedTurn {
    /// Stable `promptId` from the transcript. Acts as the dedup key when the
    /// same logical turn shows up across multiple user entries.
    pub prompt_id: String,
    /// 0-based index in arrival order.
    pub turn_index: u32,
    /// Timestamp of the first user entry with this `prompt_id`.
    pub timestamp: Option<String>,
    /// User-typed prompt (or slash-command stub). Post-redaction is the
    /// caller's responsibility — `prov backfill` runs every prompt through
    /// `Redactor::redact` before staging.
    pub prompt: String,
}

/// One AI edit reconstructed from an assistant `tool_use` block.
#[derive(Debug, Clone)]
pub struct ParsedEdit {
    /// `tool_use.id`, when present.
    pub tool_use_id: Option<String>,
    /// `Edit`, `Write`, or `MultiEdit`. Other tool names are filtered before
    /// reaching this struct.
    pub tool_name: String,
    /// Absolute or relative `file_path` from `tool_input`. Resolution to a
    /// repo-relative form happens at match time (the work tree is known
    /// there).
    pub file: String,
    /// `old_string` for `Edit`, empty for `Write`. For `MultiEdit`, one
    /// `ParsedEdit` per inner edit (matches the live-capture decomposition).
    pub old_string: String,
    /// `new_string` for `Edit`/`MultiEdit`, `content` for `Write`. The
    /// content this struct represents — the matcher hashes it line-by-line
    /// against commit diffs.
    pub new_string: String,
    /// Timestamp of the assistant entry.
    pub timestamp: Option<String>,
    /// Model from the assistant entry, when present.
    pub model: Option<String>,
    /// `turn_index` of the most-recent user turn at parse time. Defaults to
    /// 0 if no user turn preceded this edit (defensive — would indicate an
    /// unusual transcript shape).
    pub turn_index: u32,
}

/// Errors raised by the transcript parser.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    /// I/O error reading the transcript file.
    #[error("transcript I/O error: {0}")]
    Io(String),
}

/// Parse a single Claude Code session transcript file.
///
/// Best-effort: malformed JSON lines are silently skipped. Returns
/// `ParsedSession` with empty turns/edits if the file is empty or carries no
/// recognizable events.
pub fn parse_transcript(path: impl AsRef<Path>) -> Result<ParsedSession, TranscriptError> {
    let raw =
        std::fs::read_to_string(path.as_ref()).map_err(|e| TranscriptError::Io(e.to_string()))?;
    Ok(parse_transcript_text(&raw))
}

/// Parse a transcript from in-memory text. Exposed for tests and for callers
/// that already have the bytes.
#[must_use]
pub fn parse_transcript_text(raw: &str) -> ParsedSession {
    let mut session = ParsedSession {
        session_id: String::new(),
        started_at: None,
        ended_at: None,
        model: None,
        cwd: None,
        turns: Vec::new(),
        edits: Vec::new(),
    };
    let mut state = TurnState::default();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        update_session_metadata(&mut session, &obj);

        match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "user" => handle_user_event(&mut session, &mut state, &obj),
            "assistant" => handle_assistant_event(&mut session, &state, &obj),
            _ => {}
        }
    }
    session
}

#[derive(Default)]
struct TurnState {
    current_turn: u32,
    have_user_turn: bool,
    seen_prompt_ids: std::collections::HashSet<String>,
}

fn update_session_metadata(session: &mut ParsedSession, obj: &Value) {
    if session.session_id.is_empty() {
        if let Some(sid) = obj.get("sessionId").and_then(|v| v.as_str()) {
            session.session_id = sid.to_string();
        }
    }
    if session.cwd.is_none() {
        if let Some(cwd) = obj.get("cwd").and_then(|v| v.as_str()) {
            if !cwd.is_empty() {
                session.cwd = Some(cwd.to_string());
            }
        }
    }
    if let Some(ts) = obj.get("timestamp").and_then(|v| v.as_str()) {
        if session.started_at.is_none() {
            session.started_at = Some(ts.to_string());
        }
        session.ended_at = Some(ts.to_string());
    }
}

fn handle_user_event(session: &mut ParsedSession, state: &mut TurnState, obj: &Value) {
    if obj.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
        return;
    }
    let Some(prompt_id) = obj.get("promptId").and_then(|v| v.as_str()) else {
        return;
    };
    if state.seen_prompt_ids.contains(prompt_id) {
        return;
    }
    let Some(prompt) = extract_user_prompt(obj) else {
        return;
    };
    state.seen_prompt_ids.insert(prompt_id.to_string());
    let timestamp = obj
        .get("timestamp")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let turn_index = u32::try_from(session.turns.len()).unwrap_or(u32::MAX);
    session.turns.push(ParsedTurn {
        prompt_id: prompt_id.to_string(),
        turn_index,
        timestamp,
        prompt,
    });
    state.current_turn = turn_index;
    state.have_user_turn = true;
}

fn handle_assistant_event(session: &mut ParsedSession, state: &TurnState, obj: &Value) {
    let model_here = obj
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if let Some(m) = &model_here {
        if !m.is_empty() {
            session.model = Some(m.clone());
        }
    }
    let timestamp = obj
        .get("timestamp")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let attached_turn = if state.have_user_turn {
        state.current_turn
    } else {
        0
    };
    let Some(blocks) = obj
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|v| v.as_array())
    else {
        return;
    };
    for block in blocks {
        let Some(kind) = block.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if kind != "tool_use" {
            continue;
        }
        let tool_name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let input = block.get("input").unwrap_or(&Value::Null);
        let tool_use_id = block.get("id").and_then(|v| v.as_str()).map(str::to_string);
        let mut new_edits = decompose_tool_use(
            tool_name,
            input,
            tool_use_id.as_deref(),
            timestamp.as_deref(),
            model_here.as_deref(),
            attached_turn,
        );
        session.edits.append(&mut new_edits);
    }
}

/// Extract the user prompt text from a `type: "user"` entry.
///
/// `message.content` is either:
/// - a plain string (typical for typed prompts and slash-command stubs); or
/// - an array of content blocks. Accept the entry only when at least one
///   `text` block is present and no `tool_result` block is — tool-result
///   echoes are framework chatter, not user intent.
fn extract_user_prompt(obj: &Value) -> Option<String> {
    let content = obj.get("message").and_then(|m| m.get("content"))?;
    if let Some(s) = content.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    let arr = content.as_array()?;
    let has_tool_result = arr.iter().any(|b| {
        b.get("type")
            .and_then(|v| v.as_str())
            .is_some_and(|k| k == "tool_result")
    });
    if has_tool_result {
        return None;
    }
    let mut buf = String::new();
    for block in arr {
        let Some(kind) = block.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        if kind != "text" {
            continue;
        }
        let Some(text) = block.get("text").and_then(|v| v.as_str()) else {
            continue;
        };
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(text);
    }
    let buf = buf.trim().to_string();
    (!buf.is_empty()).then_some(buf)
}

/// Decompose one assistant `tool_use` block into one or more `ParsedEdit`s.
///
/// Mirrors `hook::decompose_tool_use` so backfilled edits look indistinguishable
/// from live-captured ones at the schema layer (one `ParsedEdit` per inner edit
/// for `MultiEdit`, `Write` collapses to a single edit with empty `old_string`).
fn decompose_tool_use(
    tool_name: &str,
    input: &Value,
    tool_use_id: Option<&str>,
    timestamp: Option<&str>,
    model: Option<&str>,
    turn_index: u32,
) -> Vec<ParsedEdit> {
    match tool_name {
        "Edit" => {
            let Some(file) = input.get("file_path").and_then(|v| v.as_str()) else {
                return Vec::new();
            };
            let old = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            vec![ParsedEdit {
                tool_use_id: tool_use_id.map(str::to_string),
                tool_name: tool_name.to_string(),
                file: file.to_string(),
                old_string: old.to_string(),
                new_string: new.to_string(),
                timestamp: timestamp.map(str::to_string),
                model: model.map(str::to_string),
                turn_index,
            }]
        }
        "Write" => {
            let Some(file) = input.get("file_path").and_then(|v| v.as_str()) else {
                return Vec::new();
            };
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            vec![ParsedEdit {
                tool_use_id: tool_use_id.map(str::to_string),
                tool_name: tool_name.to_string(),
                file: file.to_string(),
                old_string: String::new(),
                new_string: content.to_string(),
                timestamp: timestamp.map(str::to_string),
                model: model.map(str::to_string),
                turn_index,
            }]
        }
        "MultiEdit" => {
            let Some(file) = input.get("file_path").and_then(|v| v.as_str()) else {
                return Vec::new();
            };
            let Some(arr) = input.get("edits").and_then(|v| v.as_array()) else {
                return Vec::new();
            };
            arr.iter()
                .map(|e| {
                    let old = e.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
                    let new = e.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                    ParsedEdit {
                        tool_use_id: tool_use_id.map(str::to_string),
                        tool_name: tool_name.to_string(),
                        file: file.to_string(),
                        old_string: old.to_string(),
                        new_string: new.to_string(),
                        timestamp: timestamp.map(str::to_string),
                        model: model.map(str::to_string),
                        turn_index,
                    }
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_session_with_one_edit() {
        let raw = r#"{"type":"user","sessionId":"sess-1","promptId":"p1","timestamp":"2026-05-01T10:00:00Z","cwd":"/tmp/repo","message":{"role":"user","content":"add a greeting"}}
{"type":"assistant","sessionId":"sess-1","timestamp":"2026-05-01T10:00:05Z","message":{"model":"claude-sonnet-4-7","role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Edit","input":{"file_path":"/tmp/repo/src/main.rs","old_string":"fn main()","new_string":"fn main() { println!(\"hi\"); }"}}]}}
"#;
        let s = parse_transcript_text(raw);
        assert_eq!(s.session_id, "sess-1");
        assert_eq!(s.cwd.as_deref(), Some("/tmp/repo"));
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-7"));
        assert_eq!(s.turns.len(), 1);
        assert_eq!(s.turns[0].prompt, "add a greeting");
        assert_eq!(s.edits.len(), 1);
        assert_eq!(s.edits[0].file, "/tmp/repo/src/main.rs");
        assert_eq!(s.edits[0].turn_index, 0);
        assert_eq!(s.edits[0].tool_use_id.as_deref(), Some("toolu_1"));
    }

    #[test]
    fn skips_tool_result_user_entries() {
        let raw = r#"{"type":"user","sessionId":"x","promptId":"p1","timestamp":"2026-05-01T10:00:00Z","message":{"role":"user","content":"first"}}
{"type":"user","sessionId":"x","promptId":"p1","timestamp":"2026-05-01T10:00:01Z","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"output"}]}}
"#;
        let s = parse_transcript_text(raw);
        assert_eq!(s.turns.len(), 1);
        assert_eq!(s.turns[0].prompt, "first");
    }

    #[test]
    fn skips_meta_user_entries() {
        let raw = r#"{"type":"user","sessionId":"x","promptId":"p1","isMeta":true,"timestamp":"2026-05-01T10:00:00Z","message":{"role":"user","content":"<local-command-caveat>...</local-command-caveat>"}}
{"type":"user","sessionId":"x","promptId":"p2","timestamp":"2026-05-01T10:00:01Z","message":{"role":"user","content":"real prompt"}}
"#;
        let s = parse_transcript_text(raw);
        assert_eq!(s.turns.len(), 1);
        assert_eq!(s.turns[0].prompt, "real prompt");
    }

    #[test]
    fn dedups_user_entries_by_prompt_id() {
        // Two user entries share a promptId — the second is treated as part of
        // the same logical turn and skipped.
        let raw = r#"{"type":"user","sessionId":"x","promptId":"p1","timestamp":"2026-05-01T10:00:00Z","message":{"role":"user","content":"first"}}
{"type":"user","sessionId":"x","promptId":"p1","timestamp":"2026-05-01T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"expansion"}]}}
"#;
        let s = parse_transcript_text(raw);
        assert_eq!(s.turns.len(), 1);
        assert_eq!(s.turns[0].prompt, "first");
    }

    #[test]
    fn parses_text_content_blocks_when_no_tool_result() {
        let raw = r#"{"type":"user","sessionId":"x","promptId":"p1","timestamp":"2026-05-01T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"hello world"}]}}"#;
        let s = parse_transcript_text(raw);
        assert_eq!(s.turns.len(), 1);
        assert_eq!(s.turns[0].prompt, "hello world");
    }

    #[test]
    fn decomposes_multiedit() {
        let raw = r#"{"type":"user","sessionId":"x","promptId":"p1","timestamp":"2026-05-01T10:00:00Z","message":{"role":"user","content":"refactor"}}
{"type":"assistant","sessionId":"x","timestamp":"2026-05-01T10:00:05Z","message":{"model":"claude-sonnet-4-7","role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"MultiEdit","input":{"file_path":"/tmp/repo/lib.rs","edits":[{"old_string":"a","new_string":"A"},{"old_string":"b","new_string":"B"}]}}]}}
"#;
        let s = parse_transcript_text(raw);
        assert_eq!(s.edits.len(), 2);
        assert_eq!(s.edits[0].old_string, "a");
        assert_eq!(s.edits[0].new_string, "A");
        assert_eq!(s.edits[1].new_string, "B");
        assert!(s.edits.iter().all(|e| e.tool_name == "MultiEdit"));
    }

    #[test]
    fn write_collapses_to_single_edit_with_empty_old_string() {
        let raw = r#"{"type":"user","sessionId":"x","promptId":"p1","timestamp":"2026-05-01T10:00:00Z","message":{"role":"user","content":"new file"}}
{"type":"assistant","sessionId":"x","timestamp":"2026-05-01T10:00:05Z","message":{"model":"claude-opus-4-7","role":"assistant","content":[{"type":"tool_use","id":"toolu_3","name":"Write","input":{"file_path":"/tmp/repo/new.rs","content":"fn one() {}\n"}}]}}
"#;
        let s = parse_transcript_text(raw);
        assert_eq!(s.edits.len(), 1);
        assert_eq!(s.edits[0].old_string, "");
        assert_eq!(s.edits[0].new_string, "fn one() {}\n");
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let raw = "this is not json\n{not valid either}\n{\"type\":\"user\",\"sessionId\":\"y\",\"promptId\":\"p1\",\"timestamp\":\"2026-05-01T10:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"survives\"}}\n";
        let s = parse_transcript_text(raw);
        assert_eq!(s.session_id, "y");
        assert_eq!(s.turns.len(), 1);
    }

    #[test]
    fn ignores_unrelated_event_types() {
        let raw = r#"{"type":"permission-mode","sessionId":"z"}
{"type":"file-history-snapshot"}
{"type":"summary","summary":"unused"}
{"type":"user","sessionId":"z","promptId":"p1","timestamp":"2026-05-01T10:00:00Z","message":{"role":"user","content":"ok"}}
"#;
        let s = parse_transcript_text(raw);
        assert_eq!(s.session_id, "z");
        assert_eq!(s.turns.len(), 1);
    }

    #[test]
    fn empty_input_returns_empty_session() {
        let s = parse_transcript_text("");
        assert!(s.session_id.is_empty());
        assert!(s.turns.is_empty());
        assert!(s.edits.is_empty());
    }

    #[test]
    fn ignores_non_edit_tools() {
        let raw = r#"{"type":"user","sessionId":"x","promptId":"p1","timestamp":"2026-05-01T10:00:00Z","message":{"role":"user","content":"check"}}
{"type":"assistant","sessionId":"x","timestamp":"2026-05-01T10:00:05Z","message":{"model":"m","role":"assistant","content":[{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}]}}
"#;
        let s = parse_transcript_text(raw);
        assert!(s.edits.is_empty());
    }

    #[test]
    fn started_and_ended_track_first_and_last_timestamps() {
        let raw = r#"{"type":"user","sessionId":"x","promptId":"p1","timestamp":"2026-05-01T10:00:00Z","message":{"role":"user","content":"a"}}
{"type":"user","sessionId":"x","promptId":"p2","timestamp":"2026-05-01T11:00:00Z","message":{"role":"user","content":"b"}}
"#;
        let s = parse_transcript_text(raw);
        assert_eq!(s.started_at.as_deref(), Some("2026-05-01T10:00:00Z"));
        assert_eq!(s.ended_at.as_deref(), Some("2026-05-01T11:00:00Z"));
    }
}
