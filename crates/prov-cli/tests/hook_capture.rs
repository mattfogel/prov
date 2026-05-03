//! End-to-end capture-pipeline tests.
//!
//! Each test sets up a fixture git repo, fires `prov hook ...` invocations
//! against it via `assert_cmd`, and inspects the resulting staging tree (or
//! note ref). Tests deliberately avoid mocking Claude Code internals; they
//! drive the binary the same way the real Claude Code harness would.

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
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

#[test]
fn end_to_end_post_commit_refreshes_sqlite_cache() {
    // R3 promises sub-50ms warm-cache reads. If post-commit wrote the note
    // to git but never updated `<git-dir>/prov.db`, `prov log <file>` and
    // `prov search` after a commit would return "no provenance" until the
    // user manually ran `prov reindex`.
    //
    // We deliberately skip `prov install` here — installing would put a
    // git post-commit hook on disk that fires during `git commit` itself
    // (running whichever `prov` binary happens to be on the test process's
    // PATH, not `target/debug/prov`). That double-fire would do all the
    // work before our explicit `fire_hook("post-commit")` runs, masking
    // whether the upsert wiring is actually in play. Initializing the
    // cache via `prov reindex` gives us the file we need without putting
    // a self-firing hook on disk.
    let tmp = init_repo();
    let root = tmp.path();

    // Parent commit so post-commit's HEAD~1 diff has content to compare.
    std::fs::write(root.join("README.md"), "# bootstrap\n").unwrap();
    run_git(root, &["add", "README.md"]);
    run_git(root, &["commit", "-q", "-m", "chore: bootstrap"]);

    // Initialize the cache file (`<git-dir>/prov.db`). The post-commit
    // handler's defensive contract is to no-op silently if the cache file
    // is missing, so without this the upsert path would never run.
    prov()
        .current_dir(root)
        .arg("reindex")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .assert()
        .success();

    fire_hook(root, "session-start", &read_fixture("session-start.json"));
    fire_hook(
        root,
        "user-prompt-submit",
        &read_fixture("user-prompt-submit.json"),
    );
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
    run_git(root, &["add", "src/lib.rs"]);
    run_git(root, &["commit", "-q", "-m", "feat: hello"]);
    fire_hook(root, "post-commit", "");

    // First-call cache-keyed reads should now hit. Without the upsert
    // wiring these returned "no provenance" until the user ran
    // `prov reindex` by hand.
    prov()
        .current_dir(root)
        .args(["log", "src/lib.rs"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello function"));

    // FTS path is independent of the per-file lookup; assert it too so a
    // future regression can't quietly affect one without the other.
    prov()
        .current_dir(root)
        .args(["search", "hello"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello function"));
}

#[test]
fn note_records_per_turn_model_from_transcript_not_session_start_model() {
    // Regression for issue #36: SessionStart only fires once per session, so a
    // `/model` switch mid-session would otherwise mis-attribute every later
    // edit. The PostToolUse handler reads the most recent assistant entry from
    // the transcript and writes that model onto the EditRecord; the
    // post-commit flush prefers it over SessionMeta.model.
    let tmp = init_repo();
    let root = tmp.path();

    // Session starts in opus (the model field of session-start.json fixture).
    fire_hook(root, "session-start", &read_fixture("session-start.json"));
    fire_hook(
        root,
        "user-prompt-submit",
        &read_fixture("user-prompt-submit.json"),
    );

    // Materialize the agent's edit on disk.
    std::fs::create_dir_all(root.join("src")).unwrap();
    let after = "pub fn hello() -> &'static str {\n    \"hello, prov\"\n}\n";
    std::fs::write(root.join("src/lib.rs"), after).unwrap();

    // The transcript contains an assistant message stamped with the model the
    // user actually switched to. Real Claude Code transcripts mix message
    // types; include a non-assistant entry to prove the scan skips it.
    let transcript = root.join(".transcript.jsonl");
    let transcript_body = format!(
        "{}\n{}\n",
        r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
        r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6","role":"assistant","content":[]}}"#,
    );
    std::fs::write(&transcript, transcript_body).unwrap();

    let payload = serde_json::json!({
        "session_id": SID,
        "tool_name": "Write",
        "tool_use_id": "toolu_model",
        "transcript_path": transcript.to_string_lossy(),
        "tool_input": {
            "file_path": root.join("src/lib.rs").to_string_lossy(),
            "content": after,
        },
        "tool_response": {},
    })
    .to_string();
    fire_hook(root, "post-tool-use", &payload);
    fire_hook(root, "stop", &read_fixture("stop.json"));

    run_git(root, &["add", "src/lib.rs"]);
    run_git(root, &["commit", "-q", "-m", "feat: hello"]);
    fire_hook(root, "post-commit", "");

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
    let body = String::from_utf8(show.stdout).unwrap();
    assert!(
        body.contains("\"model\": \"claude-sonnet-4-6\""),
        "note should carry the per-turn model from the transcript, not SessionStart's model: {body}"
    );
}

#[test]
fn note_falls_back_to_session_start_model_when_transcript_unavailable() {
    // Defensive fallback: a missing or unreadable transcript must not leave
    // the note with an empty/missing model — the SessionStart-captured model
    // is the legacy behavior and remains the safety net.
    let tmp = init_repo();
    let root = tmp.path();

    fire_hook(root, "session-start", &read_fixture("session-start.json"));
    fire_hook(
        root,
        "user-prompt-submit",
        &read_fixture("user-prompt-submit.json"),
    );

    std::fs::create_dir_all(root.join("src")).unwrap();
    let after = "pub fn hello() -> &'static str {\n    \"hello, prov\"\n}\n";
    std::fs::write(root.join("src/lib.rs"), after).unwrap();

    // Point at a transcript that doesn't exist — the helper returns None.
    let payload = serde_json::json!({
        "session_id": SID,
        "tool_name": "Write",
        "tool_use_id": "toolu_fallback",
        "transcript_path": "/no/such/transcript.jsonl",
        "tool_input": {
            "file_path": root.join("src/lib.rs").to_string_lossy(),
            "content": after,
        },
        "tool_response": {},
    })
    .to_string();
    fire_hook(root, "post-tool-use", &payload);
    fire_hook(root, "stop", &read_fixture("stop.json"));

    run_git(root, &["add", "src/lib.rs"]);
    run_git(root, &["commit", "-q", "-m", "feat: hello"]);
    fire_hook(root, "post-commit", "");

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
    let body = String::from_utf8(show.stdout).unwrap();
    // session-start.json fixture's model field is the SessionStart fallback.
    let expected = read_fixture("session-start.json");
    let model_value: serde_json::Value = serde_json::from_str(&expected).unwrap();
    let session_start_model = model_value
        .get("model")
        .and_then(|v| v.as_str())
        .expect("session-start fixture missing model field");
    assert!(
        body.contains(&format!("\"model\": \"{session_start_model}\"")),
        "note should fall back to SessionStart model when transcript is unavailable: {body}"
    );
}

#[test]
fn end_to_end_capture_handles_absolute_file_path_in_tool_input() {
    // Real Claude Code passes absolute paths in `file_path`. Earlier the
    // matcher keyed on those absolute paths and `git diff` keyed on relative
    // paths, so nothing matched and post-commit silently produced no note.
    // Mirror the live shape exactly.
    let tmp = init_repo();
    let root = tmp.path();
    let abs_file = root.join("src/lib.rs");

    fire_hook(root, "session-start", &read_fixture("session-start.json"));
    fire_hook(
        root,
        "user-prompt-submit",
        &read_fixture("user-prompt-submit.json"),
    );

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        &abs_file,
        "pub fn hello() -> &'static str {\n    \"hello, prov\"\n}\n",
    )
    .unwrap();

    let payload = serde_json::json!({
        "session_id": SID,
        "tool_name": "Write",
        "tool_use_id": "toolu_abs",
        "tool_input": {
            "file_path": abs_file.to_string_lossy(),
            "content": "pub fn hello() -> &'static str {\n    \"hello, prov\"\n}\n",
        },
        "tool_response": {},
    })
    .to_string();
    fire_hook(root, "post-tool-use", &payload);
    fire_hook(root, "stop", &read_fixture("stop.json"));

    run_git(root, &["add", "src/lib.rs"]);
    run_git(root, &["commit", "-q", "-m", "feat: hello"]);
    fire_hook(root, "post-commit", "");

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
    // Note's `file` must be the relative path even though tool_input was absolute.
    assert!(
        body.contains("\"file\": \"src/lib.rs\""),
        "note should store relative file path: {body}"
    );
    assert!(
        !body.contains(&abs_file.to_string_lossy().to_string()),
        "note must not leak the absolute path: {body}"
    );
}
