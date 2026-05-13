//! Validates the repo-owned Codex hook template that `prov install --agent codex`
//! writes into project-local Codex config.

use std::path::{Path, PathBuf};

use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn read_json(path: &Path) -> Value {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

#[test]
fn codex_hooks_register_capture_events() {
    let path = repo_root().join("codex/hooks/hooks.json");
    let value = read_json(&path);
    let hooks = value
        .get("hooks")
        .and_then(Value::as_object)
        .expect("hooks.json must have top-level hooks object");

    for (event, matcher, command) in [
        (
            "SessionStart",
            Some("startup|resume|clear"),
            "prov hook codex session-start",
        ),
        (
            "UserPromptSubmit",
            None,
            "prov hook codex user-prompt-submit",
        ),
        (
            "PostToolUse",
            Some("Edit|Write"),
            "prov hook codex post-tool-use",
        ),
        ("Stop", None, "prov hook codex stop"),
    ] {
        let entries = hooks
            .get(event)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("hooks.json missing event `{event}`"));
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.get("matcher").and_then(Value::as_str), matcher);
        let nested = entry
            .get("hooks")
            .and_then(Value::as_array)
            .expect("event entry must have nested hooks");
        assert_eq!(nested.len(), 1);
        assert_eq!(
            nested[0].get("type").and_then(Value::as_str),
            Some("command")
        );
        assert_eq!(
            nested[0].get("command").and_then(Value::as_str),
            Some(command)
        );
        assert_eq!(nested[0].get("timeout").and_then(Value::as_i64), Some(5));
    }
}
