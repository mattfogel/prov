//! End-to-end tests for U7's privacy surface: `# prov:private` routing in the
//! capture pipeline, `prov mark-private`, and `prov redact-history`.
//!
//! Each test sets up a fixture git repo, drives the binary directly, and
//! inspects the resulting refs / cache / staging tree.

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use tempfile::TempDir;

use prov_core::git::Git;
use prov_core::schema::{Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

const SID: &str = "sess_fixture001";

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

fn git_capture(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git")
}

fn head_sha(cwd: &Path) -> String {
    let out = git_capture(cwd, &["rev-parse", "HEAD"]);
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn init_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    run_git(root, &["init", "-q", "-b", "main"]);
    run_git(root, &["config", "--local", "user.email", "t@x.com"]);
    run_git(root, &["config", "--local", "user.name", "T"]);
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

fn fire_hook(cwd: &Path, event: &str, payload: &str) {
    prov_in(cwd)
        .args(["hook", event])
        .write_stdin(payload.to_string())
        .assert()
        .success();
}

fn make_edit(file: &str, prompt: &str, start: u32, hashes: Vec<String>) -> Edit {
    let len = u32::try_from(hashes.len()).unwrap();
    Edit {
        file: file.into(),
        line_range: [start, start + len.saturating_sub(1)],
        content_hashes: hashes,
        original_blob_sha: None,
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

/// Initialize a repo with one commit + a note attached on the public ref.
/// Mirrors what the capture pipeline would have produced. Returns the temp
/// dir, the head SHA, and the file's BLAKE3 hashes.
fn repo_with_public_note(prompt: &str, file: &str) -> (TempDir, String) {
    let tmp = init_repo();
    let root = tmp.path();
    let body = "// alpha\n// beta\n// gamma\n";
    let path = root.join(file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, body).unwrap();
    run_git(root, &["add", file]);
    run_git(root, &["commit", "-q", "-m", "init"]);
    let head = head_sha(root);

    let real_hashes: Vec<String> = body
        .lines()
        .map(|l| blake3::hash(l.as_bytes()).to_hex().to_string())
        .collect();

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store
        .write(
            &head,
            &Note::new(vec![make_edit(file, prompt, 1, real_hashes)]),
        )
        .unwrap();

    prov_in(root).arg("reindex").assert().success();
    (tmp, head)
}

// ---------------- # prov:private routing ----------------

#[test]
fn prov_private_first_line_routes_turn_to_private_subdir() {
    let tmp = init_repo();
    let payload = serde_json::json!({
        "session_id": SID,
        "prompt": "# prov:private\nrefactor the auth middleware",
    })
    .to_string();
    fire_hook(tmp.path(), "user-prompt-submit", &payload);

    let session_dir = tmp.path().join(".git/prov-staging").join(SID);
    assert!(session_dir.join("private/turn-0.json").exists());
    assert!(!session_dir.join("turn-0.json").exists());
}

#[test]
fn prov_private_is_case_insensitive() {
    for variant in ["# Prov:Private", "# PROV:PRIVATE", "#  prov:private  "] {
        let tmp = init_repo();
        let payload = serde_json::json!({
            "session_id": SID,
            "prompt": format!("{variant}\nbody text"),
        })
        .to_string();
        fire_hook(tmp.path(), "user-prompt-submit", &payload);

        let session_dir = tmp.path().join(".git/prov-staging").join(SID);
        assert!(
            session_dir.join("private/turn-0.json").exists(),
            "variant {variant:?} did not route to private"
        );
    }
}

#[test]
fn prov_private_with_no_edits_still_records_turn_marker() {
    // *Edge case from the plan* — a turn marked `# prov:private` that produced
    // no PostToolUse edits. Nothing to flush, but the turn marker should still
    // appear in staging so future audits can confirm the opt-out fired.
    let tmp = init_repo();
    let payload = serde_json::json!({
        "session_id": SID,
        "prompt": "# prov:private\njust thinking out loud, no edits",
    })
    .to_string();
    fire_hook(tmp.path(), "user-prompt-submit", &payload);
    fire_hook(
        tmp.path(),
        "stop",
        &serde_json::json!({ "session_id": SID }).to_string(),
    );

    let priv_dir = tmp
        .path()
        .join(".git/prov-staging")
        .join(SID)
        .join("private");
    assert!(priv_dir.join("turn-0.json").exists());
    assert!(!priv_dir.join("edits.jsonl").exists());
}

#[test]
fn end_to_end_private_turn_writes_to_private_ref_only() {
    let tmp = init_repo();
    let root = tmp.path();
    // Prior commit so post-commit's HEAD~1 diff has content to compare.
    std::fs::write(root.join("README.md"), "# bootstrap\n").unwrap();
    run_git(root, &["add", "README.md"]);
    run_git(root, &["commit", "-q", "-m", "chore: bootstrap"]);

    fire_hook(
        root,
        "session-start",
        &serde_json::json!({ "session_id": SID, "model": "claude-opus-4-7" }).to_string(),
    );
    // Mark the turn private via the magic phrase.
    let prompt_payload = serde_json::json!({
        "session_id": SID,
        "prompt": "# prov:private\nadd hello function",
    })
    .to_string();
    fire_hook(root, "user-prompt-submit", &prompt_payload);

    // Materialize the file and stage a Write tool-use against it.
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn hello() -> &'static str {\n    \"hello, prov\"\n}\n",
    )
    .unwrap();
    let write_payload = serde_json::json!({
        "session_id": SID,
        "tool_name": "Write",
        "tool_use_id": "toolu_priv",
        "tool_input": {
            "file_path": "src/lib.rs",
            "content": "pub fn hello() -> &'static str {\n    \"hello, prov\"\n}\n",
        },
    })
    .to_string();
    fire_hook(root, "post-tool-use", &write_payload);
    fire_hook(
        root,
        "stop",
        &serde_json::json!({ "session_id": SID }).to_string(),
    );

    run_git(root, &["add", "src/lib.rs"]);
    run_git(root, &["commit", "-q", "-m", "feat: hello"]);
    fire_hook(root, "post-commit", "");

    let head = head_sha(root);
    let private = git_capture(root, &["notes", "--ref", NOTES_REF_PRIVATE, "show", &head]);
    let public = git_capture(root, &["notes", "--ref", NOTES_REF_PUBLIC, "show", &head]);
    assert!(
        private.status.success(),
        "expected note on private ref: {}",
        String::from_utf8_lossy(&private.stderr)
    );
    assert!(
        !public.status.success(),
        "private content leaked onto public ref"
    );
}

// ---------------- prov mark-private ----------------

#[test]
fn mark_private_moves_note_and_log_still_resolves_locally() {
    let (tmp, head) = repo_with_public_note("the original public prompt", "src/lib.rs");
    let root = tmp.path();

    prov_in(root)
        .args(["mark-private", &head])
        .assert()
        .success()
        .stdout(predicate::str::contains("moved note"));

    // Public ref no longer carries the note.
    let public = git_capture(root, &["notes", "--ref", NOTES_REF_PUBLIC, "show", &head]);
    assert!(!public.status.success(), "public note should be gone");

    // Private ref now carries it.
    let private = git_capture(root, &["notes", "--ref", NOTES_REF_PRIVATE, "show", &head]);
    assert!(private.status.success());
    let body = String::from_utf8(private.stdout).unwrap();
    assert!(body.contains("the original public prompt"));

    // `prov log` still resolves locally — the resolver overlays the private
    // ref into the cache via the no-stamp upsert.
    prov_in(root)
        .args(["log", "src/lib.rs:1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("the original public prompt"));
}

#[test]
fn mark_private_on_commit_with_no_note_exits_zero() {
    let tmp = init_repo();
    let root = tmp.path();
    std::fs::write(root.join("a.txt"), "x\n").unwrap();
    run_git(root, &["add", "a.txt"]);
    run_git(root, &["commit", "-q", "-m", "x"]);
    let head = head_sha(root);

    prov_in(root)
        .args(["mark-private", &head])
        .assert()
        .success()
        .stdout(predicate::str::contains("no public note"));
}

#[test]
fn mark_private_invalid_commit_errors() {
    let tmp = init_repo();
    let root = tmp.path();
    std::fs::write(root.join("a.txt"), "x\n").unwrap();
    run_git(root, &["add", "a.txt"]);
    run_git(root, &["commit", "-q", "-m", "x"]);

    prov_in(root)
        .args(["mark-private", "not-a-real-sha"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("could not resolve commit"));
}

// ---------------- prov redact-history ----------------

#[test]
fn redact_history_rewrites_matching_prompts_and_log_shows_marker() {
    let (tmp, _head) = repo_with_public_note("Working on the Acme Corp launch deck", "src/lib.rs");
    let root = tmp.path();

    prov_in(root)
        .args(["redact-history", "Acme Corp"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rewrote 1 of 1"));

    prov_in(root)
        .args(["log", "src/lib.rs:1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[REDACTED:provignore-rule:cli]"))
        .stdout(predicate::str::contains("Acme Corp").not());
}

#[test]
fn redact_history_with_no_matches_reports_zero_rewrites() {
    let (tmp, _head) = repo_with_public_note("no sensitive content here", "src/lib.rs");
    let root = tmp.path();

    prov_in(root)
        .args(["redact-history", "MISSING_PATTERN"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 notes rewritten"));
}

#[test]
fn redact_history_invalid_regex_errors_before_rewriting() {
    let (tmp, head) = repo_with_public_note("prompt body", "src/lib.rs");
    let root = tmp.path();

    prov_in(root)
        .args(["redact-history", "[unbalanced"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid regex"));

    // Public ref untouched.
    let public = git_capture(root, &["notes", "--ref", NOTES_REF_PUBLIC, "show", &head]);
    assert!(public.status.success());
    let body = String::from_utf8(public.stdout).unwrap();
    assert!(body.contains("prompt body"));
}

#[test]
fn redact_history_also_scrubs_private_ref() {
    // Plant a note directly on the private ref and confirm redact-history
    // touches it. The user moved the note to private *because* it was
    // sensitive; the late-arriving pattern should still apply there.
    let tmp = init_repo();
    let root = tmp.path();
    std::fs::write(root.join("a.txt"), "x\n").unwrap();
    run_git(root, &["add", "a.txt"]);
    run_git(root, &["commit", "-q", "-m", "x"]);
    let head = head_sha(root);

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PRIVATE);
    store
        .write(
            &head,
            &Note::new(vec![make_edit(
                "a.txt",
                "leaks Acme Corp internal codename",
                1,
                vec![blake3::hash(b"x").to_hex().to_string()],
            )]),
        )
        .unwrap();

    prov_in(root).arg("reindex").assert().success();
    prov_in(root)
        .args(["redact-history", "Acme Corp"])
        .assert()
        .success()
        .stdout(predicate::str::contains("rewrote 1 of 1"));

    let private = git_capture(root, &["notes", "--ref", NOTES_REF_PRIVATE, "show", &head]);
    assert!(private.status.success());
    let body = String::from_utf8(private.stdout).unwrap();
    assert!(body.contains("[REDACTED:provignore-rule:cli]"));
    assert!(!body.contains("Acme Corp"));
}
