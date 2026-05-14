//! Agent-hooks bundle layout tests.
//!
//! Validates the on-disk shape of `agent-hooks/` — the directory whose
//! `hooks.json` is embedded by `prov install --agent claude` into a repo's
//! `.claude/settings.json`. The previous shape lived under `plugin/` and
//! carried a Claude Code plugin manifest; the manifest is gone, but the
//! hooks bundle itself is still load-bearing and worth lint-testing.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Walks up from `prov-cli`'s manifest dir to the workspace root.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root resolves from prov-cli manifest dir")
        .to_path_buf()
}

fn agent_hooks_dir() -> PathBuf {
    workspace_root().join("agent-hooks")
}

fn read_json(path: &Path) -> Value {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("expected {} to exist: {e}", path.display());
    });
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        panic!("{} is not valid JSON: {e}", path.display());
    })
}

#[test]
fn agent_hooks_register_all_four_events() {
    let hooks_path = agent_hooks_dir().join("hooks.json");
    let value = read_json(&hooks_path);
    let hooks = value
        .get("hooks")
        .and_then(Value::as_object)
        .expect("hooks.json must have a top-level `hooks` object");

    let expected: &[(&str, Option<&str>, &str)] = &[
        ("SessionStart", None, "prov hook session-start"),
        ("UserPromptSubmit", None, "prov hook user-prompt-submit"),
        (
            "PostToolUse",
            Some("Edit|Write|MultiEdit"),
            "prov hook post-tool-use",
        ),
        ("Stop", None, "prov hook stop"),
    ];

    for (event, matcher, command) in expected {
        let entries = hooks
            .get(*event)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("hooks.json missing event `{event}`"));
        assert_eq!(
            entries.len(),
            1,
            "event `{event}` should have exactly one entry"
        );
        let entry = &entries[0];

        if let Some(expected_matcher) = matcher {
            assert_eq!(
                entry.get("matcher").and_then(Value::as_str),
                Some(*expected_matcher),
                "event `{event}` matcher mismatch"
            );
        }

        let nested = entry
            .get("hooks")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("event `{event}` should have nested `hooks` array"));
        assert_eq!(
            nested.len(),
            1,
            "event `{event}` should have exactly one nested hook"
        );
        let nested_hook = &nested[0];
        assert_eq!(
            nested_hook.get("type").and_then(Value::as_str),
            Some("command"),
            "event `{event}` hook should be of type `command`"
        );
        assert_eq!(
            nested_hook.get("command").and_then(Value::as_str),
            Some(*command),
            "event `{event}` command mismatch"
        );
        // 5-second timeout for every capture hook.
        assert_eq!(
            nested_hook.get("timeout").and_then(Value::as_i64),
            Some(5),
            "event `{event}` hook should have timeout: 5"
        );
    }
}

#[test]
fn agent_hooks_readme_exists() {
    let readme = agent_hooks_dir().join("README.md");
    assert!(
        readme.exists(),
        "agent-hooks/README.md must exist so the directory is self-explanatory"
    );
}
