//! End-to-end tests for U5's read-side CLI: install, uninstall, log, search,
//! reindex, pr-timeline. Each test sets up a fresh fixture git repo, drives
//! the binary via `assert_cmd`, and inspects the resulting tree.

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use tempfile::TempDir;

use prov_core::git::Git;
use prov_core::schema::{Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::NOTES_REF_PUBLIC;

fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_capture(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git capture");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn init_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    run_git(tmp.path(), &["init", "-q", "-b", "main"]);
    run_git(tmp.path(), &["config", "--local", "user.email", "t@x.com"]);
    run_git(tmp.path(), &["config", "--local", "user.name", "T"]);
    tmp
}

fn prov() -> AssertCommand {
    AssertCommand::cargo_bin("prov").unwrap()
}

fn prov_in(cwd: &Path) -> AssertCommand {
    let mut c = prov();
    c.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    c
}

fn make_edit(file: &str, prompt: &str, start: u32, hashes: Vec<String>) -> Edit {
    let len = u32::try_from(hashes.len()).unwrap();
    Edit {
        file: file.into(),
        line_range: [start, start + len - 1],
        content_hashes: hashes,
        original_blob_sha: Some("blob".into()),
        prompt: prompt.into(),
        conversation_id: "sess_fixture".into(),
        turn_index: 0,
        tool_use_id: None,
        preceding_turns_summary: None,
        model: "claude-sonnet-4-5".into(),
        tool: "claude-code".into(),
        timestamp: "2026-04-28T12:00:00Z".into(),
        derived_from: None,
    }
}

// ---------------- install / uninstall ----------------

#[test]
fn install_in_fresh_repo_writes_hooks_settings_and_cache() {
    let tmp = init_repo();

    prov_in(tmp.path()).arg("install").assert().success();

    // Post-commit hook installed and contains the prov block.
    let hook = std::fs::read_to_string(tmp.path().join(".git/hooks/post-commit")).unwrap();
    assert!(hook.contains("# >>> prov"));
    assert!(hook.contains("prov hook post-commit"));
    assert!(hook.contains("# <<< prov"));

    // Pre-push hook (U8 secret-scanning gate) installed too.
    let pre_push = std::fs::read_to_string(tmp.path().join(".git/hooks/pre-push")).unwrap();
    assert!(pre_push.contains("# >>> prov"));
    assert!(pre_push.contains("prov hook pre-push"));
    assert!(pre_push.contains("# <<< prov"));

    // .claude/settings.json contains all four hook events, each emitted in
    // Claude Code's required entry shape: `{ matcher?, hooks: [{type, command,
    // timeout?}] }`. The earlier shape (top-level `command` on the entry)
    // parsed as JSON but was rejected by Claude Code at session start.
    let settings = std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&settings).unwrap();
    for event in ["UserPromptSubmit", "PostToolUse", "Stop", "SessionStart"] {
        let arr = v["hooks"][event]
            .as_array()
            .unwrap_or_else(|| panic!("settings.json missing event {event}: {settings}"));
        let prov_block = arr
            .iter()
            .find(|entry| {
                entry["hooks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|h| h["command"].as_str())
                    .any(|cmd| cmd.starts_with("prov hook"))
            })
            .unwrap_or_else(|| panic!("no prov-owned block for {event}: {settings}"));
        // Top-level `command` would fail Claude Code's schema validation.
        assert!(
            prov_block["command"].is_null(),
            "prov entry for {event} must not carry top-level `command`: {prov_block}"
        );
        let inner = prov_block["hooks"]
            .as_array()
            .unwrap_or_else(|| panic!("prov entry for {event} missing inner `hooks` array"));
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["type"], "command");
        assert!(inner[0]["command"]
            .as_str()
            .unwrap()
            .starts_with("prov hook"));
    }

    // git config keys are set.
    assert_eq!(
        git_capture(tmp.path(), &["config", "--local", "notes.displayRef"]).trim(),
        "refs/notes/prompts"
    );
    assert_eq!(
        git_capture(tmp.path(), &["config", "--local", "notes.mergeStrategy"]).trim(),
        "manual"
    );

    // SQLite cache was created.
    assert!(tmp.path().join(".git/prov.db").exists());
}

#[test]
fn install_is_idempotent() {
    let tmp = init_repo();
    prov_in(tmp.path()).arg("install").assert().success();
    let first_hook = std::fs::read_to_string(tmp.path().join(".git/hooks/post-commit")).unwrap();
    let first_settings = std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap();

    prov_in(tmp.path()).arg("install").assert().success();
    let second_hook = std::fs::read_to_string(tmp.path().join(".git/hooks/post-commit")).unwrap();
    let second_settings =
        std::fs::read_to_string(tmp.path().join(".claude/settings.json")).unwrap();

    assert_eq!(first_hook, second_hook);
    assert_eq!(first_settings, second_settings);

    // Settings JSON should still have exactly one prov entry per event.
    let v: serde_json::Value = serde_json::from_str(&second_settings).unwrap();
    let post_tool_use = v["hooks"]["PostToolUse"].as_array().unwrap();
    assert_eq!(post_tool_use.len(), 1);
}

#[test]
fn install_preserves_user_hook_content() {
    let tmp = init_repo();
    let hook_path = tmp.path().join(".git/hooks/post-commit");
    std::fs::create_dir_all(hook_path.parent().unwrap()).unwrap();
    std::fs::write(&hook_path, "#!/bin/sh\necho user-original-hook\n").unwrap();

    prov_in(tmp.path()).arg("install").assert().success();

    let hook = std::fs::read_to_string(&hook_path).unwrap();
    assert!(hook.contains("echo user-original-hook"));
    assert!(hook.contains("# >>> prov"));
    assert!(hook.contains("prov hook post-commit"));
}

#[test]
fn install_preserves_user_claude_settings_keys() {
    let tmp = init_repo();
    let settings_path = tmp.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    // Pre-existing user content uses Claude Code's schema (matcher? + hooks
    // array). Install must preserve unrelated top-level keys, leave the user's
    // hook block alone, and append its own prov-owned block alongside.
    std::fs::write(
        &settings_path,
        r#"{"theme":"dark","hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo user"}]}]}}"#,
    )
    .unwrap();

    prov_in(tmp.path()).arg("install").assert().success();

    let raw = std::fs::read_to_string(&settings_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["theme"], "dark");
    let stop_arr = v["hooks"]["Stop"].as_array().unwrap();
    let entry_commands = |e: &serde_json::Value| -> Vec<String> {
        e["hooks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| h["command"].as_str().map(str::to_owned))
            .collect()
    };
    assert!(stop_arr
        .iter()
        .any(|e| entry_commands(e).iter().any(|c| c == "echo user")));
    assert!(stop_arr
        .iter()
        .any(|e| entry_commands(e).iter().any(|c| c == "prov hook stop")));
}

#[test]
fn install_heals_legacy_top_level_command_shape() {
    // A previous prov build wrote prov hooks with `command` at the entry top
    // level; Claude Code rejected the resulting settings.json on load. A
    // re-install must replace those legacy entries with the schema-correct
    // shape rather than appending alongside them.
    let tmp = init_repo();
    let settings_path = tmp.path().join(".claude/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    std::fs::write(
        &settings_path,
        r#"{"hooks":{"Stop":[{"command":"prov hook stop","timeout":5}]}}"#,
    )
    .unwrap();

    prov_in(tmp.path()).arg("install").assert().success();

    let raw = std::fs::read_to_string(&settings_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let stop_arr = v["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(
        stop_arr.len(),
        1,
        "legacy prov entry should be replaced, not duplicated"
    );
    assert!(
        stop_arr[0]["command"].is_null(),
        "post-heal entry must not carry top-level `command`: {stop_arr:?}"
    );
    assert_eq!(stop_arr[0]["hooks"][0]["command"], "prov hook stop");
}

#[test]
fn install_plugin_flag_does_not_touch_repo() {
    let tmp = init_repo();
    prov_in(tmp.path())
        .args(["install", "--plugin"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/plugin install prov"));

    assert!(!tmp.path().join(".git/hooks/post-commit").exists());
    assert!(!tmp.path().join(".claude/settings.json").exists());
}

#[test]
fn install_enable_push_configures_fetch_refspec() {
    let tmp = init_repo();
    run_git(
        tmp.path(),
        &["remote", "add", "origin", "https://example.com/repo.git"],
    );

    prov_in(tmp.path())
        .args(["install", "--enable-push", "origin"])
        .assert()
        .success();

    let fetches = git_capture(
        tmp.path(),
        &["config", "--local", "--get-all", "remote.origin.fetch"],
    );
    assert!(
        fetches
            .lines()
            .any(|l| l == "refs/notes/prompts:refs/notes/origin/prompts"),
        "expected prov refspec; got: {fetches}"
    );
}

#[test]
fn install_outside_repo_errors_cleanly() {
    let tmp = TempDir::new().unwrap();
    prov_in(tmp.path())
        .arg("install")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not in a git repo"));
}

#[test]
fn uninstall_round_trips_install() {
    let tmp = init_repo();
    let hook_path = tmp.path().join(".git/hooks/post-commit");
    std::fs::create_dir_all(hook_path.parent().unwrap()).unwrap();
    std::fs::write(&hook_path, "#!/bin/sh\necho user-original\n").unwrap();

    prov_in(tmp.path()).arg("install").assert().success();
    prov_in(tmp.path()).arg("uninstall").assert().success();

    let hook = std::fs::read_to_string(&hook_path).unwrap();
    assert!(!hook.contains("# >>> prov"));
    assert!(hook.contains("echo user-original"));

    // Pre-push hook had no user content, so it gets removed entirely.
    assert!(!tmp.path().join(".git/hooks/pre-push").exists());

    // Settings file removed when no other content is present.
    assert!(!tmp.path().join(".claude/settings.json").exists());

    // Cache preserved without --purge.
    assert!(tmp.path().join(".git/prov.db").exists());
    // Git config keys are unset.
    let display_ref = Command::new("git")
        .current_dir(tmp.path())
        .args(["config", "--local", "notes.displayRef"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap();
    assert!(!display_ref.success());
}

#[test]
fn uninstall_purge_deletes_cache_and_staging() {
    let tmp = init_repo();
    prov_in(tmp.path()).arg("install").assert().success();
    std::fs::create_dir_all(tmp.path().join(".git/prov-staging")).unwrap();
    std::fs::write(tmp.path().join(".git/prov-staging/log"), "x").unwrap();

    prov_in(tmp.path())
        .args(["uninstall", "--purge"])
        .assert()
        .success();

    assert!(!tmp.path().join(".git/prov.db").exists());
    assert!(!tmp.path().join(".git/prov-staging").exists());
}

#[test]
fn uninstall_when_not_installed_is_a_noop() {
    let tmp = init_repo();
    prov_in(tmp.path()).arg("uninstall").assert().success();
}

// ---------------- log / search / reindex / pr-timeline ----------------

/// Initialize a repo with a single commit, write a note attached to HEAD via
/// the public NotesStore API, and reindex the cache. The fixture mirrors what
/// the capture pipeline (U3) would have produced and gives the read CLI
/// realistic input.
fn repo_with_note(prompt: &str, file: &str, line: u32, hashes: &[String]) -> (TempDir, String) {
    use std::fmt::Write as _;
    let tmp = init_repo();

    // Seed the file with `line` lines of content matching the hashes.
    let mut body = String::new();
    for h in hashes {
        writeln!(body, "// content {h}").unwrap();
    }
    std::fs::write(tmp.path().join(file), body).unwrap();
    run_git(tmp.path(), &["add", file]);
    run_git(tmp.path(), &["commit", "-q", "-m", "initial"]);
    let sha = git_capture(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    // Compute real BLAKE3 hashes for each line so the resolver reports `Unchanged`.
    let real_hashes: Vec<String> = (0..hashes.len())
        .map(|i| {
            let line_text = format!("// content {}", hashes[i]);
            blake3::hash(line_text.as_bytes()).to_hex().to_string()
        })
        .collect();

    let git = Git::discover(tmp.path()).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let _ = hashes; // hashes count is captured via real_hashes; suppress unused-binding lint.
    store
        .write(
            &sha,
            &Note::new(vec![make_edit(file, prompt, line, real_hashes)]),
        )
        .unwrap();

    // Run reindex via the CLI so the cache is populated as the user would see it.
    prov_in(tmp.path()).arg("reindex").assert().success();

    (tmp, sha)
}

#[test]
fn log_point_lookup_returns_unchanged_for_matching_line() {
    let (tmp, _sha) = repo_with_note(
        "use a 24h dedupe window",
        "payments.rs",
        1,
        &["a".into(), "b".into(), "c".into()],
    );
    prov_in(tmp.path())
        .args(["log", "payments.rs:1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unchanged"))
        .stdout(predicate::str::contains("use a 24h dedupe window"));
}

#[test]
fn log_point_lookup_no_provenance_when_line_outside_range() {
    let (tmp, _sha) = repo_with_note("p", "f.rs", 1, &["a".into(), "b".into(), "c".into()]);
    prov_in(tmp.path())
        .args(["log", "f.rs:99"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no provenance"));
}

#[test]
fn log_whole_file_lists_edits_with_json_envelope() {
    let (tmp, _sha) = repo_with_note("wholefile", "x.rs", 1, &["a".into(), "b".into()]);
    let out = prov_in(tmp.path())
        .args(["log", "x.rs", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["file"], "x.rs");
    let edits = v["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["prompt"], "wholefile");
}

#[test]
fn log_only_if_substantial_returns_empty_for_short_files() {
    let (tmp, _sha) = repo_with_note("short", "tiny.rs", 1, &["a".into(), "b".into()]);
    // tiny.rs has 2 lines (plus trailing newline) — under the substantial threshold.
    prov_in(tmp.path())
        .args(["log", "tiny.rs", "--only-if-substantial", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"edits\":[]"));
}

#[test]
fn search_returns_match_for_indexed_prompt() {
    let (tmp, _sha) = repo_with_note(
        "Stripe webhook dedupe",
        "f.rs",
        1,
        &["a".into(), "b".into()],
    );
    prov_in(tmp.path())
        .args(["search", "stripe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Stripe webhook dedupe"));
}

#[test]
fn search_handles_query_with_fts_metacharacters() {
    // FTS5's bare `-` would otherwise be parsed as NOT.
    let (tmp, _sha) = repo_with_note(
        "the dash-prefixed flag broke",
        "f.rs",
        1,
        &["a".into(), "b".into()],
    );
    prov_in(tmp.path())
        .args(["search", "--", "-prefixed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dash-prefixed"));
}

#[test]
fn reindex_on_repo_with_no_notes_prints_message() {
    let tmp = init_repo();
    std::fs::write(tmp.path().join("README"), "x").unwrap();
    run_git(tmp.path(), &["add", "README"]);
    run_git(tmp.path(), &["commit", "-q", "-m", "init"]);

    prov_in(tmp.path())
        .arg("reindex")
        .assert()
        .success()
        .stdout(predicate::str::contains("no notes to index"));
}

#[test]
fn pr_timeline_renders_markdown_for_resolved_lines() {
    let (tmp, _sha) = repo_with_note(
        "Add Stripe webhook handling",
        "p.rs",
        1,
        &["a".into(), "b".into(), "c".into()],
    );
    // base = empty tree (root parent); head = HEAD. We can't use empty tree
    // directly for `git diff base..head`, so create a base-ref by tagging the
    // initial commit's parent — except there is no parent. Use the magic
    // "empty tree" object instead.
    let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let out = prov_in(tmp.path())
        .args([
            "pr-timeline",
            "--base",
            empty_tree,
            "--head",
            "HEAD",
            "--markdown",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let md = String::from_utf8(out).unwrap();
    assert!(md.contains("<!-- prov:pr-timeline -->"));
    assert!(md.contains("PR Intent Timeline"));
    assert!(md.contains("Add Stripe webhook handling"));
    assert!(md.contains("p.rs (3 lines)"));
}

#[test]
fn pr_timeline_json_envelope_is_valid() {
    let (tmp, _sha) = repo_with_note("json shape", "f.rs", 1, &["a".into(), "b".into()]);
    let empty_tree = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let out = prov_in(tmp.path())
        .args([
            "pr-timeline",
            "--base",
            empty_tree,
            "--head",
            "HEAD",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["sessions"][0]["turns"][0]["prompt"], "json shape");
    assert_eq!(v["total_turns"], 1);
}
