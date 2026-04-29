//! End-to-end capture-pipeline tests.
//!
//! Each test sets up a fixture git repo, fires `prov hook ...` invocations
//! against it via `assert_cmd`, and inspects the resulting staging tree (or
//! note ref). Tests deliberately avoid mocking Claude Code internals; they
//! drive the binary the same way the real Claude Code harness would.

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use tempfile::TempDir;

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

fn fire_hook(cwd: &Path, event: &str, payload: &str) {
    prov()
        .current_dir(cwd)
        .args(["hook", event])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .write_stdin(payload)
        .assert()
        .success();
}

fn fire_post_rewrite(cwd: &Path, kind: &str, payload: &str) {
    prov()
        .current_dir(cwd)
        .args(["hook", "post-rewrite", kind])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .write_stdin(payload)
        .assert()
        .success();
}

fn read_fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("prov-core")
        .join("tests")
        .join("fixtures")
        .join("hook-payloads")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn staging_path(repo: &Path) -> std::path::PathBuf {
    repo.join(".git").join("prov-staging")
}

#[test]
fn user_prompt_submit_creates_turn_file() {
    let tmp = init_repo();
    fire_hook(
        tmp.path(),
        "user-prompt-submit",
        &read_fixture("user-prompt-submit.json"),
    );

    let session_dir = staging_path(tmp.path()).join(SID);
    assert!(session_dir.exists());
    let turn_path = session_dir.join("turn-0.json");
    assert!(turn_path.exists(), "turn-0.json not written");

    let body = std::fs::read_to_string(turn_path).unwrap();
    assert!(body.contains("\"turn_index\": 0"));
    assert!(body.contains("\"private\": false"));
    assert!(body.contains("hello function"));
}

#[test]
fn user_prompt_submit_with_prov_private_routes_to_private_dir() {
    let tmp = init_repo();
    let payload = serde_json::json!({
        "session_id": SID,
        "prompt": "# prov:private\nrefactor the auth middleware\n",
    })
    .to_string();
    fire_hook(tmp.path(), "user-prompt-submit", &payload);

    let session_dir = staging_path(tmp.path()).join(SID);
    let private_turn = session_dir.join("private").join("turn-0.json");
    let public_turn = session_dir.join("turn-0.json");
    assert!(private_turn.exists(), "private turn missing");
    assert!(
        !public_turn.exists(),
        "private content leaked into public dir"
    );
}

#[test]
fn user_prompt_submit_redacts_secrets_before_staging() {
    let tmp = init_repo();
    let payload = serde_json::json!({
        "session_id": SID,
        "prompt": "the AWS key is AKIAIOSFODNN7EXAMPLE",
    })
    .to_string();
    fire_hook(tmp.path(), "user-prompt-submit", &payload);

    let body =
        std::fs::read_to_string(staging_path(tmp.path()).join(SID).join("turn-0.json")).unwrap();
    assert!(!body.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(body.contains("[REDACTED:aws-key]"));
}

#[test]
fn post_tool_use_multiedit_decomposes_to_one_record_per_inner_edit() {
    let tmp = init_repo();
    fire_hook(
        tmp.path(),
        "user-prompt-submit",
        &read_fixture("user-prompt-submit.json"),
    );
    fire_hook(
        tmp.path(),
        "post-tool-use",
        &read_fixture("post-tool-use-multiedit.json"),
    );

    let edits_path = staging_path(tmp.path()).join(SID).join("edits.jsonl");
    let body = std::fs::read_to_string(&edits_path).unwrap();
    let lines: Vec<&str> = body.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3);
}

#[test]
fn empty_stdin_does_not_crash() {
    let tmp = init_repo();
    // Empty stdin: the handler treats it as `{}` and exits 0 silently.
    fire_hook(tmp.path(), "user-prompt-submit", "");
}

#[test]
fn malformed_json_stdin_does_not_crash() {
    let tmp = init_repo();
    // Malformed payload: hook subcommand exits 0 (defensive contract); error
    // is logged. We just assert exit success.
    prov()
        .current_dir(tmp.path())
        .args(["hook", "user-prompt-submit"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .write_stdin("not json")
        .assert()
        .success();
}

#[test]
fn outside_git_repo_is_silent_noop() {
    let tmp = TempDir::new().unwrap();
    // Not a git repo. Hook subcommand exits 0 immediately.
    prov()
        .current_dir(tmp.path())
        .args(["hook", "user-prompt-submit"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .write_stdin(read_fixture("user-prompt-submit.json"))
        .assert()
        .success();
    // No staging dir should have been created (because no .git dir exists).
    assert!(!tmp.path().join(".git").exists());
}

#[test]
fn post_rewrite_is_a_typed_noop_in_v1() {
    let tmp = init_repo();
    // U9 will fill this in; for now exit 0 cleanly with a typed kind arg.
    fire_post_rewrite(tmp.path(), "amend", "");
    fire_post_rewrite(tmp.path(), "rebase", "");
}

#[test]
fn pre_push_is_a_typed_noop_in_v1() {
    let tmp = init_repo();
    // U8 fills this in. Exits 0 cleanly today.
    fire_hook(tmp.path(), "pre-push", "");
}

#[test]
fn end_to_end_capture_writes_note_to_head_on_post_commit() {
    let tmp = init_repo();
    let root = tmp.path();

    // Drive the full Claude Code session lifecycle for one turn that writes
    // src/lib.rs, then a real `git add && git commit`, then post-commit.
    fire_hook(root, "session-start", &read_fixture("session-start.json"));
    fire_hook(
        root,
        "user-prompt-submit",
        &read_fixture("user-prompt-submit.json"),
    );

    // Materialize the file the agent "wrote" so the diff has content to match.
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn hello() -> &'static str {\n    \"hello, prov\"\n}\n",
    )
    .unwrap();

    fire_hook(
        root,
        "post-tool-use",
        &read_fixture("post-tool-use-write.json"),
    );
    fire_hook(root, "stop", &read_fixture("stop.json"));

    // Real commit so HEAD has a parent path the post-commit handler can diff.
    run_git(root, &["add", "src/lib.rs"]);
    run_git(root, &["commit", "-q", "-m", "feat: hello"]);

    // Trigger the post-commit handler explicitly (we don't install the git
    // hook in this test; we drive the same code path directly).
    fire_hook(root, "post-commit", "");

    // Verify a note was written to refs/notes/prompts attached to HEAD.
    let head = String::from_utf8(
        Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();

    let show = Command::new("git")
        .current_dir(root)
        .args(["notes", "--ref", "refs/notes/prompts", "show", &head])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "no note attached to HEAD: stderr={}",
        String::from_utf8_lossy(&show.stderr)
    );
    let body = String::from_utf8(show.stdout).unwrap();
    assert!(
        body.contains("\"version\""),
        "note JSON missing version: {body}"
    );
    assert!(
        body.contains("hello function"),
        "note missing prompt text: {body}"
    );
    assert!(
        body.contains("\"file\": \"src/lib.rs\""),
        "note missing file: {body}"
    );
}
