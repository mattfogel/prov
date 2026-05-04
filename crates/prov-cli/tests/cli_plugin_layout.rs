//! U11 plugin-layout tests.
//!
//! Validates the on-disk shape of `plugin/` against the documented Claude
//! Code plugin schema, plus the behavioral guarantee that
//! `prov install --plugin` exits without mutating the project's `.claude/`.

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

fn plugin_dir() -> PathBuf {
    workspace_root().join("plugin")
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
fn plugin_manifest_has_required_fields() {
    let manifest = plugin_dir().join(".claude-plugin").join("plugin.json");
    let value = read_json(&manifest);
    let obj = value
        .as_object()
        .expect("plugin.json must be a JSON object");

    // The Claude Code plugin schema requires `name` at minimum; we additionally
    // require `description` and `version` so the marketplace listing has
    // enough metadata to render usefully.
    for required in ["name", "description", "version"] {
        assert!(
            obj.contains_key(required),
            "plugin.json is missing required field `{required}`"
        );
        assert!(
            obj[required].is_string() && !obj[required].as_str().unwrap().is_empty(),
            "plugin.json field `{required}` must be a non-empty string"
        );
    }

    assert_eq!(
        obj["name"].as_str().unwrap(),
        "prov",
        "plugin name must be `prov` (matches binary name and marketplace install command)"
    );
}

#[test]
fn plugin_hooks_register_all_four_events() {
    let hooks_path = plugin_dir().join("hooks").join("hooks.json");
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
        // Plan specifies a 5-second timeout for every capture hook.
        assert_eq!(
            nested_hook.get("timeout").and_then(Value::as_i64),
            Some(5),
            "event `{event}` hook should have timeout: 5"
        );
    }
}

#[test]
fn plugin_readme_exists() {
    let readme = plugin_dir().join("README.md");
    assert!(
        readme.exists(),
        "plugin/README.md must exist so marketplace listings have install instructions"
    );
}

// Behavioral coverage for `prov install --plugin` not mutating `.claude/`
// lives in `cli_read.rs::install_plugin_flag_does_not_touch_repo` — the U5
// install tests own that fixture setup and we don't duplicate it here.
