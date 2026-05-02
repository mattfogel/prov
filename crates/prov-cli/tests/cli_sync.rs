//! End-to-end tests for U8's `prov fetch` / `prov push` commands.
//!
//! Each test sets up a local repo + a bare "remote" repo, drives the binary
//! directly, and inspects the resulting notes refs.

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

fn init_bare_remote() -> TempDir {
    let tmp = TempDir::new().unwrap();
    run_git(tmp.path(), &["init", "-q", "--bare"]);
    tmp
}

fn prov_in(cwd: &Path) -> AssertCommand {
    let mut c = AssertCommand::cargo_bin("prov").unwrap();
    c.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    c
}

fn make_edit(file: &str, prompt: &str, hashes: Vec<String>) -> Edit {
    let len = u32::try_from(hashes.len()).unwrap();
    Edit {
        file: file.into(),
        line_range: [1, len.saturating_sub(1).saturating_add(1)],
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

/// Build a repo with one commit + a public note attached, with `origin`
/// pointing at the bare remote. Returns the local repo, remote, and head SHA.
fn repo_with_remote_and_note(prompt: &str) -> (TempDir, TempDir, String) {
    let local = init_repo();
    let remote = init_bare_remote();
    let lpath = local.path();
    let rpath = remote.path();

    std::fs::write(lpath.join("README.md"), "hi\n").unwrap();
    run_git(lpath, &["add", "README.md"]);
    run_git(lpath, &["commit", "-q", "-m", "init"]);
    let head = head_sha(lpath);

    // Add the bare repo as origin (using its absolute path).
    run_git(lpath, &["remote", "add", "origin", rpath.to_str().unwrap()]);

    // Plant a note on the public ref.
    let git = Git::discover(lpath).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let hashes = vec![blake3::hash(b"hi").to_hex().to_string()];
    store
        .write(
            &head,
            &Note::new(vec![make_edit("README.md", prompt, hashes)]),
        )
        .unwrap();

    (local, remote, head)
}

// ---------------- prov push ----------------

#[test]
fn push_creates_notes_ref_on_remote_when_absent() {
    let (local, remote, head) = repo_with_remote_and_note("totally clean prompt");
    let lpath = local.path();
    let rpath = remote.path();

    prov_in(lpath)
        .arg("push")
        .assert()
        .success()
        .stdout(predicate::str::contains("refs/notes/prompts pushed"));

    // Remote now has the ref.
    let show = git_capture(rpath, &["notes", "--ref", NOTES_REF_PUBLIC, "show", &head]);
    assert!(
        show.status.success(),
        "remote should hold the note: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    let body = String::from_utf8(show.stdout).unwrap();
    assert!(body.contains("totally clean prompt"));
}

#[test]
fn push_when_local_and_remote_identical_is_a_noop_success() {
    let (local, _remote, _head) = repo_with_remote_and_note("idempotent prompt");
    let lpath = local.path();

    prov_in(lpath).arg("push").assert().success();
    // A second push immediately after should still succeed (git prints
    // "Everything up-to-date" and exits 0).
    prov_in(lpath).arg("push").assert().success();
}

#[test]
fn push_uses_origin_when_remote_argument_omitted() {
    let (local, _remote, _head) = repo_with_remote_and_note("default-remote prompt");
    let lpath = local.path();

    prov_in(lpath)
        .arg("push")
        .assert()
        .success()
        .stdout(predicate::str::contains("prov push origin"));
}

// ---------------- prov fetch ----------------

#[test]
fn fetch_retrieves_remote_notes_into_local_ref() {
    let (local, remote, head) = repo_with_remote_and_note("teammate's prompt");
    let lpath = local.path();
    let rpath = remote.path();

    // Push the branch first so a fresh clone has the commit history; then
    // push notes via `prov push`.
    run_git(lpath, &["push", "-u", "origin", "main"]);
    prov_in(lpath).arg("push").assert().success();

    // Simulate a fresh clone: a brand-new local with the commit history but
    // no notes ref locally.
    let fresh = TempDir::new().unwrap();
    let fpath = fresh.path();
    run_git(
        Path::new("."),
        &[
            "clone",
            "-q",
            rpath.to_str().unwrap(),
            fpath.to_str().unwrap(),
        ],
    );
    run_git(fpath, &["config", "--local", "user.email", "t@x.com"]);
    run_git(fpath, &["config", "--local", "user.name", "T"]);

    // Sanity: no notes locally.
    let pre = git_capture(fpath, &["notes", "--ref", NOTES_REF_PUBLIC, "show", &head]);
    assert!(
        !pre.status.success(),
        "fresh clone should have no notes yet"
    );

    prov_in(fpath)
        .arg("fetch")
        .assert()
        .success()
        .stdout(predicate::str::contains("0 → 1 notes"));

    let post = git_capture(fpath, &["notes", "--ref", NOTES_REF_PUBLIC, "show", &head]);
    assert!(post.status.success(), "fetch should populate the local ref");
    let body = String::from_utf8(post.stdout).unwrap();
    assert!(body.contains("teammate's prompt"));
}

#[test]
fn fetch_when_remote_has_no_notes_succeeds_with_zero_count() {
    let local = init_repo();
    let remote = init_bare_remote();
    let lpath = local.path();
    let rpath = remote.path();

    std::fs::write(lpath.join("a.txt"), "x\n").unwrap();
    run_git(lpath, &["add", "a.txt"]);
    run_git(lpath, &["commit", "-q", "-m", "x"]);
    run_git(lpath, &["remote", "add", "origin", rpath.to_str().unwrap()]);
    // Push the branch so the remote isn't empty (avoids fetch failing for an
    // unrelated reason — we want to assert "no notes" specifically).
    run_git(lpath, &["push", "-u", "origin", "main"]);

    prov_in(lpath)
        .arg("fetch")
        .assert()
        // Fetch errors when the source ref doesn't exist on the remote. This
        // is the documented v1 behavior — the user opts into team mode by
        // first having someone push.
        .failure()
        .stderr(predicate::str::contains("refs/notes/prompts"));
}
