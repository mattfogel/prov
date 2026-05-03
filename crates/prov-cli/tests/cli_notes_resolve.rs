//! End-to-end tests for U10's `prov notes-resolve` command.
//!
//! Each scenario builds a real two-clone fixture (or simulates the post-fetch
//! merge-in-progress state directly) and drives the binary, then asserts on
//! the resulting notes-ref state and `git notes show` output.

mod common;

use std::path::Path;
use std::process::Command;

use predicates::prelude::*;
use tempfile::TempDir;

use prov_core::git::Git;
use prov_core::schema::{Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::NOTES_REF_PUBLIC;

use common::{git_capture, head_sha, init_bare_remote, prov_in, run_git};

/// Build an Edit with the given identity fields. Body fields (file, hashes)
/// derive from `id` so two distinct ids produce visually distinct notes.
fn edit_with_id(id: &str, conv: &str, turn: u32, ts: &str) -> Edit {
    Edit {
        file: format!("file-{id}.rs"),
        line_range: [1, 1],
        content_hashes: vec![format!("hash-{id}")],
        original_blob_sha: None,
        prompt: format!("prompt from {id}"),
        conversation_id: conv.into(),
        turn_index: turn,
        tool_use_id: Some(format!("tool_{id}")),
        preceding_turns_summary: None,
        model: "claude-sonnet-4-5".into(),
        tool: "claude-code".into(),
        timestamp: ts.into(),
        derived_from: None,
    }
}

/// Set up two clones of a shared bare remote, each with a single seed commit
/// already pushed. Returns (dev_a_path, dev_b_path, remote, head_sha).
fn two_dev_fixture() -> (TempDir, TempDir, TempDir, String) {
    let remote = init_bare_remote();
    let rpath = remote.path();

    // Dev A creates the seed commit and pushes.
    let dev_a = TempDir::new().unwrap();
    let apath = dev_a.path();
    run_git(apath, &["init", "-q", "-b", "main"]);
    run_git(apath, &["config", "--local", "user.email", "a@x.com"]);
    run_git(apath, &["config", "--local", "user.name", "A"]);
    std::fs::write(apath.join("README.md"), "shared\n").unwrap();
    run_git(apath, &["add", "README.md"]);
    run_git(apath, &["commit", "-q", "-m", "seed"]);
    run_git(apath, &["remote", "add", "origin", rpath.to_str().unwrap()]);
    run_git(apath, &["push", "-u", "origin", "main"]);
    let head = head_sha(apath);

    // Dev B clones to get the same seed commit.
    let dev_b = TempDir::new().unwrap();
    let bpath = dev_b.path();
    run_git(
        Path::new("."),
        &[
            "clone",
            "-q",
            rpath.to_str().unwrap(),
            bpath.to_str().unwrap(),
        ],
    );
    run_git(bpath, &["config", "--local", "user.email", "b@x.com"]);
    run_git(bpath, &["config", "--local", "user.name", "B"]);

    (dev_a, dev_b, remote, head)
}

#[test]
fn no_merge_in_progress_exits_zero_with_message() {
    let dir = TempDir::new().unwrap();
    let path = dir.path();
    run_git(path, &["init", "-q", "-b", "main"]);
    run_git(path, &["config", "--local", "user.email", "t@x.com"]);
    run_git(path, &["config", "--local", "user.name", "T"]);

    prov_in(path)
        .arg("notes-resolve")
        .assert()
        .success()
        .stdout(predicate::str::contains("no merge to resolve"));
}

#[test]
fn two_dev_disjoint_edits_unioned_after_resolve() {
    let (dev_a, dev_b, _remote, head) = two_dev_fixture();
    let apath = dev_a.path();
    let bpath = dev_b.path();

    // Both devs install prov so they have notes.mergeStrategy=manual and the
    // origin fetch refspec wired (--enable-push owns the latter).
    install_prov_with_push(apath, "origin");
    install_prov_with_push(bpath, "origin");

    // Dev A annotates turn 0 of session-a, pushes.
    plant_note(
        apath,
        &head,
        &Note::new(vec![edit_with_id("a", "sess_a", 0, "2026-04-28T10:00:00Z")]),
    );
    prov_in(apath).arg("push").assert().success();

    // Dev B annotates turn 5 of session-b — completely disjoint key — and
    // tries to push, which fails (notes ref diverged on the remote). Dev B
    // fetches, which produces an in-progress merge.
    plant_note(
        bpath,
        &head,
        &Note::new(vec![edit_with_id("b", "sess_b", 5, "2026-04-28T11:00:00Z")]),
    );

    prov_in(bpath).arg("fetch").assert().failure(); // merge produced a conflict
    assert!(
        bpath.join(".git/NOTES_MERGE_REF").exists(),
        "fetch should leave a notes merge in progress for dev B"
    );

    // Resolve and verify the merged note now contains both edits.
    prov_in(bpath)
        .arg("notes-resolve")
        .assert()
        .success()
        .stdout(predicate::str::contains("finalized"));
    assert!(
        !bpath.join(".git/NOTES_MERGE_REF").exists(),
        "successful resolve should clear NOTES_MERGE_REF"
    );

    let merged_json = String::from_utf8(
        git_capture(bpath, &["notes", "--ref", NOTES_REF_PUBLIC, "show", &head]).stdout,
    )
    .unwrap();
    let merged = Note::from_json(&merged_json).expect("valid merged note");
    assert_eq!(merged.edits.len(), 2, "both sides' edits should survive");
    assert_eq!(merged.version, 1, "merged note should keep schema v1");

    // Edits should sort by ascending timestamp — locking determinism in
    // the resolver's sort rather than relying on whatever map order
    // landed first.
    let timestamps: Vec<&str> = merged.edits.iter().map(|e| e.timestamp.as_str()).collect();
    let mut sorted = timestamps.clone();
    sorted.sort_unstable();
    assert_eq!(
        timestamps, sorted,
        "merged edits should be sorted by timestamp ascending"
    );

    // A by-conversation lookup so field-preservation asserts don't depend on
    // sort order changing under us in the future.
    let by_conv: std::collections::HashMap<&str, &Edit> = merged
        .edits
        .iter()
        .map(|e| (e.conversation_id.as_str(), e))
        .collect();

    let a = by_conv.get("sess_a").expect("sess_a edit should survive");
    assert_eq!(a.prompt, "prompt from a");
    assert_eq!(a.model, "claude-sonnet-4-5");
    assert_eq!(a.tool_use_id.as_deref(), Some("tool_a"));
    assert_eq!(a.timestamp, "2026-04-28T10:00:00Z");
    assert_eq!(a.turn_index, 0);

    let b = by_conv.get("sess_b").expect("sess_b edit should survive");
    assert_eq!(b.prompt, "prompt from b");
    assert_eq!(b.model, "claude-sonnet-4-5");
    assert_eq!(b.tool_use_id.as_deref(), Some("tool_b"));
    assert_eq!(b.timestamp, "2026-04-28T11:00:00Z");
    assert_eq!(b.turn_index, 5);
}

#[test]
fn collision_on_tool_use_id_keeps_later_timestamp() {
    let (dev_a, dev_b, _remote, head) = two_dev_fixture();
    let apath = dev_a.path();
    let bpath = dev_b.path();
    install_prov_with_push(apath, "origin");
    install_prov_with_push(bpath, "origin");

    // Both devs claim the same (conv_id, turn_index, tool_use_id) tuple — the
    // "impossible in practice" defensive case. The later timestamp wins.
    plant_note(
        apath,
        &head,
        &Note::new(vec![Edit {
            timestamp: "2026-04-28T10:00:00Z".into(),
            prompt: "earlier".into(),
            ..edit_with_id("a", "shared_sess", 0, "2026-04-28T10:00:00Z")
        }]),
    );
    let mut later = edit_with_id("b", "shared_sess", 0, "2026-04-28T12:00:00Z");
    later.tool_use_id = Some("tool_a".into()); // force collision with A
    later.prompt = "later".into();
    plant_note(bpath, &head, &Note::new(vec![later]));

    prov_in(apath).arg("push").assert().success();
    prov_in(bpath).arg("fetch").assert().failure();
    prov_in(bpath).arg("notes-resolve").assert().success();

    let merged_json = String::from_utf8(
        git_capture(bpath, &["notes", "--ref", NOTES_REF_PUBLIC, "show", &head]).stdout,
    )
    .unwrap();
    let merged = Note::from_json(&merged_json).unwrap();
    assert_eq!(merged.edits.len(), 1, "collision should collapse to one");
    assert_eq!(merged.edits[0].timestamp, "2026-04-28T12:00:00Z");
    assert_eq!(merged.edits[0].prompt, "later");
}

#[test]
fn schema_version_mismatch_aborts_cleanly() {
    let (dev_a, dev_b, _remote, head) = two_dev_fixture();
    let apath = dev_a.path();
    let bpath = dev_b.path();
    install_prov_with_push(apath, "origin");
    install_prov_with_push(bpath, "origin");

    // Dev A writes a real v1 note and pushes.
    plant_note(
        apath,
        &head,
        &Note::new(vec![edit_with_id("a", "sess_a", 0, "2026-04-28T10:00:00Z")]),
    );
    prov_in(apath).arg("push").assert().success();

    // Dev B writes a hypothetical v2 note straight to the local notes ref
    // (bypassing prov, since prov refuses to write non-current schema). This
    // simulates the upgrade-skew case the resolver must protect against.
    let v2_body = r#"{"version":2,"edits":[],"future_field":"shape changed"}"#;
    write_raw_note(bpath, &head, v2_body);

    // Fetch produces a merge-in-progress; resolve must refuse.
    prov_in(bpath).arg("fetch").assert().failure();
    prov_in(bpath)
        .arg("notes-resolve")
        .assert()
        .failure()
        .stderr(predicate::str::contains("schema version"));

    // The merge state is intact (still in progress) so the user can run
    // `git notes --ref=prompts merge --abort` per the error message.
    assert!(
        bpath.join(".git/NOTES_MERGE_REF").exists(),
        "failed resolve should preserve the merge state for the user to abort"
    );
}

// ---------------- helpers ----------------

fn install_prov_with_push(cwd: &Path, remote: &str) {
    // Prov install for both devs so they share the manual mergeStrategy.
    // --enable-push wires the fetch refspec for the named remote so prov fetch
    // can find the remote's notes ref.
    let _ = prov_in(cwd)
        .args(["install", "--enable-push", remote])
        .assert()
        .success();
}

fn plant_note(cwd: &Path, sha: &str, note: &Note) {
    let git = Git::discover(cwd).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store.write(sha, note).unwrap();
}

/// Write an arbitrary blob (here a v2-shaped note body) directly to the public
/// notes ref. Bypasses `NotesStore::write`'s schema validation so the test can
/// simulate the upgrade-skew case.
fn write_raw_note(cwd: &Path, sha: &str, body: &str) {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("git")
        .current_dir(cwd)
        .args([
            "notes",
            "--ref",
            NOTES_REF_PUBLIC,
            "add",
            "--force",
            "--file=-",
            sha,
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn git notes add");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
}
