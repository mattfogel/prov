//! End-to-end tests for U8's pre-push secret-scanning gate.
//!
//! Most cases drive `prov hook pre-push` directly with the documented
//! `<local-ref> <local-sha> <remote-ref> <remote-sha>` stdin format. One
//! integration case wires the hook through `prov install` and exercises a
//! real `prov push` against a bare remote.

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use tempfile::TempDir;

use prov_core::git::Git;
use prov_core::schema::{Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

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

fn ref_sha(cwd: &Path, ref_name: &str) -> String {
    let out = git_capture(cwd, &["rev-parse", "--verify", "-q", ref_name]);
    assert!(out.status.success(), "rev-parse {ref_name} failed");
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

fn init_bare_remote() -> TempDir {
    let tmp = TempDir::new().unwrap();
    run_git(tmp.path(), &["init", "-q", "--bare"]);
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

fn make_edit(file: &str, prompt: &str, hashes: Vec<String>) -> Edit {
    let len = u32::try_from(hashes.len()).unwrap();
    Edit {
        file: file.into(),
        line_range: [1, len.max(1)],
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

/// Build a repo with one commit and a public note carrying `prompt`. Returns
/// the temp dir, the head SHA, and the public-ref SHA.
fn repo_with_public_note(prompt: &str) -> (TempDir, String, String) {
    let tmp = init_repo();
    let root = tmp.path();
    std::fs::write(root.join("README.md"), "hi\n").unwrap();
    run_git(root, &["add", "README.md"]);
    run_git(root, &["commit", "-q", "-m", "init"]);
    let head = head_sha(root);

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let hashes = vec![blake3::hash(b"hi").to_hex().to_string()];
    store
        .write(
            &head,
            &Note::new(vec![make_edit("README.md", prompt, hashes)]),
        )
        .unwrap();
    let ref_sha = ref_sha(root, NOTES_REF_PUBLIC);
    (tmp, head, ref_sha)
}

// ---------------- direct `prov hook pre-push` tests ----------------

#[test]
fn pre_push_passes_when_no_notes_refs_are_in_stdin() {
    // Push of a regular branch — gate should be a no-op (default scoping per R6).
    let tmp = init_repo();
    let stdin = format!(
        "refs/heads/main {sha} refs/heads/main {rsha}\n",
        sha = "a".repeat(40),
        rsha = "b".repeat(40),
    );
    prov_in(tmp.path())
        .args(["hook", "pre-push"])
        .write_stdin(stdin)
        .assert()
        .success();
}

#[test]
fn pre_push_passes_when_public_notes_ref_has_no_secrets() {
    let (tmp, _head, public_sha) = repo_with_public_note("totally clean prompt");
    let stdin = format!("{NOTES_REF_PUBLIC} {public_sha} {NOTES_REF_PUBLIC} {ZERO_SHA}\n");
    prov_in(tmp.path())
        .args(["hook", "pre-push"])
        .write_stdin(stdin)
        .assert()
        .success();
}

#[test]
fn pre_push_blocks_unredacted_aws_key_in_new_note() {
    // The note body slipped past the runtime redactor (e.g., user wrote it
    // outside the capture pipeline). Pre-push is the second line of defense.
    let (tmp, head, public_sha) = repo_with_public_note("Use AKIAIOSFODNN7EXAMPLE for the deploy");
    let stdin = format!("{NOTES_REF_PUBLIC} {public_sha} {NOTES_REF_PUBLIC} {ZERO_SHA}\n");
    prov_in(tmp.path())
        .args(["hook", "pre-push"])
        .write_stdin(stdin)
        .assert()
        .failure()
        .stderr(predicate::str::contains("aws-key"))
        .stderr(predicate::str::contains(&head));
}

#[test]
fn pre_push_blocks_when_local_ref_is_private_regardless_of_remote() {
    // Catches the manual-mapping bypass:
    //   git push origin refs/notes/prompts-private:refs/notes/prompts
    let tmp = init_repo();
    let stdin = format!(
        "{NOTES_REF_PRIVATE} {a} {NOTES_REF_PUBLIC} {b}\n",
        a = "a".repeat(40),
        b = ZERO_SHA,
    );
    prov_in(tmp.path())
        .args(["hook", "pre-push"])
        .write_stdin(stdin)
        .assert()
        .failure()
        .stderr(predicate::str::contains("private notes are local-only"));
}

#[test]
fn pre_push_blocks_when_remote_ref_is_private() {
    // Mirror image: user maps a public local ref onto the private remote ref.
    let tmp = init_repo();
    let stdin = format!(
        "{NOTES_REF_PUBLIC} {a} {NOTES_REF_PRIVATE} {b}\n",
        a = "a".repeat(40),
        b = ZERO_SHA,
    );
    prov_in(tmp.path())
        .args(["hook", "pre-push"])
        .write_stdin(stdin)
        .assert()
        .failure()
        .stderr(predicate::str::contains("private notes are local-only"));
}

#[test]
fn pre_push_with_malformed_stdin_exits_zero_defensively() {
    // A bug in Prov must never break the user's push. Empty stdin and
    // truncated lines both pass through silently.
    let tmp = init_repo();
    prov_in(tmp.path())
        .args(["hook", "pre-push"])
        .write_stdin("")
        .assert()
        .success();
    prov_in(tmp.path())
        .args(["hook", "pre-push"])
        .write_stdin("garbage with too few fields\n")
        .assert()
        .success();
}

#[test]
fn pre_push_skips_deletion_of_public_ref() {
    // local-sha = zeros means "delete this remote ref" — there is no new
    // content to scan, so the gate must let it through.
    let tmp = init_repo();
    let stdin = format!(
        "{NOTES_REF_PUBLIC} {ZERO_SHA} {NOTES_REF_PUBLIC} {sha}\n",
        sha = "c".repeat(40),
    );
    prov_in(tmp.path())
        .args(["hook", "pre-push"])
        .write_stdin(stdin)
        .assert()
        .success();
}

#[test]
fn pre_push_only_flags_newly_added_blobs_when_remote_already_has_some() {
    // Plant two notes; push the first to the "remote" ref locally; then
    // verify the gate compares against the snapshot and only scans the
    // added second note.
    let tmp = init_repo();
    let root = tmp.path();
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    run_git(root, &["add", "a.txt"]);
    run_git(root, &["commit", "-q", "-m", "a"]);
    let first_head = head_sha(root);
    std::fs::write(root.join("b.txt"), "b\n").unwrap();
    run_git(root, &["add", "b.txt"]);
    run_git(root, &["commit", "-q", "-m", "b"]);
    let second_head = head_sha(root);

    let git = Git::discover(root).unwrap();
    let public = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);

    // Note 1 is clean; note 2 carries the AWS key. Both written to the public
    // ref, but the test simulates the remote already having note 1 only.
    public
        .write(
            &first_head,
            &Note::new(vec![make_edit(
                "a.txt",
                "clean prompt",
                vec![blake3::hash(b"a").to_hex().to_string()],
            )]),
        )
        .unwrap();
    let snapshot_sha = ref_sha(root, NOTES_REF_PUBLIC);

    public
        .write(
            &second_head,
            &Note::new(vec![make_edit(
                "b.txt",
                "ship with AKIAIOSFODNN7EXAMPLE",
                vec![blake3::hash(b"b").to_hex().to_string()],
            )]),
        )
        .unwrap();
    let local_sha = ref_sha(root, NOTES_REF_PUBLIC);

    let stdin = format!("{NOTES_REF_PUBLIC} {local_sha} {NOTES_REF_PUBLIC} {snapshot_sha}\n");
    let assert = prov_in(root)
        .args(["hook", "pre-push"])
        .write_stdin(stdin)
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("aws-key"), "stderr: {stderr}");
    assert!(
        stderr.contains(&second_head),
        "stderr should call out the new note's commit ({second_head}); got: {stderr}"
    );
    assert!(
        !stderr.contains(&first_head),
        "stderr should not flag the unchanged first note ({first_head}); got: {stderr}"
    );
}

// ---------------- end-to-end via `prov install` + `prov push` ----------------

#[test]
fn end_to_end_install_then_push_aborts_on_planted_secret() {
    let local = init_repo();
    let remote = init_bare_remote();
    let lpath = local.path();
    let rpath = remote.path();

    std::fs::write(lpath.join("README.md"), "hi\n").unwrap();
    run_git(lpath, &["add", "README.md"]);
    run_git(lpath, &["commit", "-q", "-m", "init"]);
    let head = head_sha(lpath);
    run_git(lpath, &["remote", "add", "origin", rpath.to_str().unwrap()]);

    // Install wires .git/hooks/pre-push to call `prov hook pre-push`.
    prov_in(lpath).arg("install").assert().success();

    // Plant a public note with an unredacted AWS key.
    let git = Git::discover(lpath).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store
        .write(
            &head,
            &Note::new(vec![make_edit(
                "README.md",
                "use AKIAIOSFODNN7EXAMPLE in the deploy script",
                vec![blake3::hash(b"hi").to_hex().to_string()],
            )]),
        )
        .unwrap();

    // The hook script does `command -v prov`; surface the cargo-built binary
    // on PATH so it runs (otherwise the hook silently passes).
    let cargo_bin = assert_cmd::cargo::cargo_bin("prov");
    let cargo_dir = cargo_bin.parent().unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    let augmented_path = format!("{}:{}", cargo_dir.display(), path);

    let assert = prov_in(lpath)
        .env("PATH", &augmented_path)
        .arg("push")
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("aws-key"),
        "expected aws-key in stderr; got: {stderr}"
    );
    assert!(
        stderr.contains(&head),
        "expected commit SHA {head} in stderr; got: {stderr}"
    );

    // Remote should not have received the note.
    let show = git_capture(rpath, &["notes", "--ref", NOTES_REF_PUBLIC, "show", &head]);
    assert!(
        !show.status.success(),
        "remote should still have no note for {head}"
    );
}

#[test]
fn no_verify_audit_logs_to_staging() {
    let local = init_repo();
    let remote = init_bare_remote();
    let lpath = local.path();
    let rpath = remote.path();

    std::fs::write(lpath.join("README.md"), "hi\n").unwrap();
    run_git(lpath, &["add", "README.md"]);
    run_git(lpath, &["commit", "-q", "-m", "init"]);
    let head = head_sha(lpath);
    run_git(lpath, &["remote", "add", "origin", rpath.to_str().unwrap()]);

    let git = Git::discover(lpath).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store
        .write(
            &head,
            &Note::new(vec![make_edit(
                "README.md",
                "ship AKIAIOSFODNN7EXAMPLE anyway",
                vec![blake3::hash(b"hi").to_hex().to_string()],
            )]),
        )
        .unwrap();

    prov_in(lpath)
        .args(["push", "--no-verify"])
        .assert()
        .success();

    let log_path = lpath.join(".git/prov-staging/log");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("--no-verify"),
        "audit log should record the bypass; got: {log}"
    );
}
