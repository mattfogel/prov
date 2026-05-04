//! Integration tests for `prov regenerate <file>:<line>`.
//!
//! Uses `mockito` to stand up a fake Anthropic Messages endpoint and
//! `PROV_ANTHROPIC_BASE_URL` (the hidden test override) to redirect the
//! client at it. Each test seeds a fixture repo with a real commit, blob,
//! and note so the resolver finds the line through the same code path as
//! production.

use std::io::Write;
use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use tempfile::TempDir;

use prov_core::git::Git;
use prov_core::schema::{DerivedFrom, Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::NOTES_REF_PUBLIC;

// ---------------- fixture helpers ----------------

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

fn prov_in(cwd: &Path) -> AssertCommand {
    let mut c = AssertCommand::cargo_bin("prov").unwrap();
    c.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    c
}

/// Hash `content` as a git blob and write it into the object database, returning
/// the resulting SHA. Mirrors `git hash-object -w --stdin`.
fn write_blob(cwd: &Path, content: &str) -> String {
    let mut child = Command::new("git")
        .current_dir(cwd)
        .args(["hash-object", "-w", "--stdin"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git hash-object");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git hash-object failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Seed a repo with a commit on `file`, store an Edit in a note attached to
/// HEAD with `original_blob_sha` pointing at `original_text`, and return the
/// repo handle plus the head SHA.
fn repo_with_regen_note(
    file: &str,
    line_count: u32,
    prompt: &str,
    model: &str,
    original_text: &str,
    derived_from: Option<DerivedFrom>,
    preceding_summary: Option<String>,
) -> TempDir {
    use std::fmt::Write as _;
    let tmp = init_repo();

    // Seed the file with `line_count` lines whose hashes we can match in the note.
    let mut body = String::new();
    for i in 0..line_count {
        writeln!(body, "// content {i}").unwrap();
    }
    let target = tmp.path().join(file);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&target, &body).unwrap();
    run_git(tmp.path(), &["add", file]);
    run_git(tmp.path(), &["commit", "-q", "-m", "initial"]);

    let head_sha = git_capture(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let blob_sha = write_blob(tmp.path(), original_text);

    let real_hashes: Vec<String> = (0..line_count)
        .map(|i| {
            let line_text = format!("// content {i}");
            blake3::hash(line_text.as_bytes()).to_hex().to_string()
        })
        .collect();

    let edit = Edit {
        file: file.into(),
        line_range: [1, line_count],
        content_hashes: real_hashes,
        original_blob_sha: Some(blob_sha),
        prompt: prompt.into(),
        conversation_id: "sess_test".into(),
        turn_index: 0,
        tool_use_id: None,
        preceding_turns_summary: preceding_summary,
        model: model.into(),
        tool: "claude-code".into(),
        timestamp: "2026-04-28T12:00:00Z".into(),
        derived_from,
    };

    let git = Git::discover(tmp.path()).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store.write(&head_sha, &Note::new(vec![edit])).unwrap();

    // Reindex so the resolver sees the note via the cache.
    prov_in(tmp.path()).arg("reindex").assert().success();

    tmp
}

/// Mount a default mockito mock for `POST /v1/messages` returning the given
/// text in a single text content block. Returns the server so the caller can
/// keep it alive for the duration of the test (the mock unmounts on drop).
fn mock_anthropic_text(text: &str) -> (mockito::ServerGuard, String) {
    let mut server = mockito::Server::new();
    let url = server.url();
    let body = serde_json::json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn"
    });
    server
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body.to_string())
        .create();
    (server, url)
}

// ---------------- tests ----------------

#[test]
fn regenerate_happy_path_renders_diff() {
    let original = "fn original() {\n    println!(\"original output\");\n}\n";
    let regenerated = "fn original() {\n    println!(\"regenerated output\");\n}\n";
    let tmp = repo_with_regen_note(
        "src/lib.rs",
        3,
        "write a fn that prints",
        "claude-haiku-4-5",
        original,
        None,
        None,
    );
    let (_server, url) = mock_anthropic_text(regenerated);

    prov_in(tmp.path())
        .args(["regenerate", "src/lib.rs:1"])
        .env("ANTHROPIC_API_KEY", "sk-ant-test-key")
        .env("PROV_ANTHROPIC_BASE_URL", url)
        .assert()
        .success()
        .stdout(predicate::str::contains("--- original (captured)"))
        .stdout(predicate::str::contains("+++ regenerated"))
        // Lines that match between original and regenerated render with a leading space.
        .stdout(predicate::str::contains(
            "-    println!(\"original output\");",
        ))
        .stdout(predicate::str::contains(
            "+    println!(\"regenerated output\");",
        ))
        // Header surfaces the captured prompt and model.
        .stdout(predicate::str::contains("write a fn that prints"))
        .stdout(predicate::str::contains("claude-haiku-4-5"));
}

#[test]
fn regenerate_errors_clearly_when_api_key_missing() {
    let tmp = repo_with_regen_note("x.rs", 2, "p", "claude-haiku-4-5", "original\n", None, None);

    prov_in(tmp.path())
        .args(["regenerate", "x.rs:1"])
        .env_remove("ANTHROPIC_API_KEY")
        .assert()
        .failure()
        .stderr(predicate::str::contains("ANTHROPIC_API_KEY is not set"));
}

#[test]
fn regenerate_surfaces_429_with_retry_after() {
    let tmp = repo_with_regen_note("x.rs", 2, "p", "claude-haiku-4-5", "original\n", None, None);

    let mut server = mockito::Server::new();
    server
        .mock("POST", "/v1/messages")
        .with_status(429)
        .with_header("retry-after", "42")
        .with_body("rate limited")
        .create();

    prov_in(tmp.path())
        .args(["regenerate", "x.rs:1"])
        .env("ANTHROPIC_API_KEY", "sk-ant-test")
        .env("PROV_ANTHROPIC_BASE_URL", server.url())
        .assert()
        .failure()
        .stderr(predicate::str::contains("rate-limited"))
        .stderr(predicate::str::contains("retry-after=42"));
}

#[test]
fn regenerate_when_blob_missing_still_calls_api_and_prints_response() {
    // Build a note that points at a non-existent blob SHA — simulating gc.
    let tmp = init_repo();
    std::fs::write(tmp.path().join("x.rs"), "// content 0\n// content 1\n").unwrap();
    run_git(tmp.path(), &["add", "x.rs"]);
    run_git(tmp.path(), &["commit", "-q", "-m", "init"]);
    let head_sha = git_capture(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    let hashes = ["// content 0", "// content 1"]
        .iter()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string())
        .collect::<Vec<_>>();

    let edit = Edit {
        file: "x.rs".into(),
        line_range: [1, 2],
        content_hashes: hashes,
        // Plausible but unreachable blob SHA.
        original_blob_sha: Some("0000000000000000000000000000000000000000".into()),
        prompt: "p".into(),
        conversation_id: "s".into(),
        turn_index: 0,
        tool_use_id: None,
        preceding_turns_summary: None,
        model: "claude-haiku-4-5".into(),
        tool: "claude-code".into(),
        timestamp: "2026-04-28T12:00:00Z".into(),
        derived_from: None,
    };
    let git = Git::discover(tmp.path()).unwrap();
    NotesStore::new(git, NOTES_REF_PUBLIC)
        .write(&head_sha, &Note::new(vec![edit]))
        .unwrap();
    prov_in(tmp.path()).arg("reindex").assert().success();

    let (_server, url) = mock_anthropic_text("regenerated body");
    prov_in(tmp.path())
        .args(["regenerate", "x.rs:1"])
        .env("ANTHROPIC_API_KEY", "sk-ant-test")
        .env("PROV_ANTHROPIC_BASE_URL", url)
        .assert()
        .success()
        .stderr(predicate::str::contains("no longer reachable"))
        .stdout(predicate::str::contains("--- regenerated output ---"))
        .stdout(predicate::str::contains("regenerated body"));
}

#[test]
fn regenerate_root_walks_derived_from_to_original_prompt() {
    // Two-commit chain. Commit A introduces x.rs (with the *original* prompt's
    // captured content); commit B rewrites the same lines (the rewrite prompt's
    // captured content). After commit B, `git blame` attributes line 1 to B,
    // so the resolver returns B's note. `--root` then walks edit_b's
    // `derived_from` back to edit_a and surfaces the original prompt.
    let tmp = init_repo();
    std::fs::write(tmp.path().join("x.rs"), "// content A\n// content X\n").unwrap();
    run_git(tmp.path(), &["add", "x.rs"]);
    run_git(tmp.path(), &["commit", "-q", "-m", "first"]);
    let sha_a = git_capture(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let blob_a = write_blob(tmp.path(), "A original\n");

    // Commit B replaces both lines so blame on either line attributes to B.
    std::fs::write(tmp.path().join("x.rs"), "// content B\n// content Y\n").unwrap();
    run_git(tmp.path(), &["add", "x.rs"]);
    run_git(tmp.path(), &["commit", "-q", "-m", "second"]);
    let sha_b = git_capture(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let blob_b = write_blob(tmp.path(), "B rewrite\n");

    // Hashes for commit A's content (note_a covers the file as it stood at A).
    let hashes_a = ["// content A", "// content X"]
        .iter()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string())
        .collect::<Vec<_>>();
    // Hashes for commit B's content (note_b covers the rewrite).
    let hashes_b = ["// content B", "// content Y"]
        .iter()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string())
        .collect::<Vec<_>>();

    let edit_a = Edit {
        file: "x.rs".into(),
        line_range: [1, 2],
        content_hashes: hashes_a,
        original_blob_sha: Some(blob_a),
        prompt: "ROOT prompt".into(),
        conversation_id: "s".into(),
        turn_index: 0,
        tool_use_id: None,
        preceding_turns_summary: None,
        model: "claude-haiku-4-5".into(),
        tool: "claude-code".into(),
        timestamp: "2026-04-28T11:00:00Z".into(),
        derived_from: None,
    };
    let edit_b = Edit {
        file: "x.rs".into(),
        line_range: [1, 2],
        content_hashes: hashes_b,
        original_blob_sha: Some(blob_b),
        prompt: "REWRITE prompt".into(),
        conversation_id: "s".into(),
        turn_index: 1,
        tool_use_id: None,
        preceding_turns_summary: None,
        model: "claude-haiku-4-5".into(),
        tool: "claude-code".into(),
        timestamp: "2026-04-28T12:00:00Z".into(),
        derived_from: Some(DerivedFrom::Rewrite {
            source_commit: sha_a.clone(),
            source_edit: 0,
        }),
    };

    let git = Git::discover(tmp.path()).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store.write(&sha_a, &Note::new(vec![edit_a])).unwrap();
    store.write(&sha_b, &Note::new(vec![edit_b])).unwrap();
    prov_in(tmp.path()).arg("reindex").assert().success();

    // Default: REWRITE prompt surfaces.
    let (server_default, url_default) = mock_anthropic_text("regenerated");
    prov_in(tmp.path())
        .args(["regenerate", "x.rs:1"])
        .env("ANTHROPIC_API_KEY", "sk-ant-test")
        .env("PROV_ANTHROPIC_BASE_URL", url_default)
        .assert()
        .success()
        .stdout(predicate::str::contains("REWRITE prompt"));
    drop(server_default);

    // --root: ROOT prompt surfaces.
    let (server_root, url_root) = mock_anthropic_text("regenerated");
    prov_in(tmp.path())
        .args(["regenerate", "x.rs:1", "--root"])
        .env("ANTHROPIC_API_KEY", "sk-ant-test")
        .env("PROV_ANTHROPIC_BASE_URL", url_root)
        .assert()
        .success()
        .stdout(predicate::str::contains("ROOT prompt"))
        .stdout(predicate::str::contains("(--root)"));
    drop(server_root);
}
