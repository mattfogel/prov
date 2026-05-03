//! End-to-end tests for `prov repair` and `prov gc`.

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use tempfile::TempDir;

use prov_core::git::Git;
use prov_core::schema::{Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::staging::Staging;
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

fn head_sha(cwd: &Path) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git");
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

fn prov_in(cwd: &Path) -> AssertCommand {
    let mut c = AssertCommand::cargo_bin("prov").unwrap();
    c.current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    c
}

fn make_edit(prompt: &str, ts: &str) -> Edit {
    Edit {
        file: "README.md".into(),
        line_range: [1, 1],
        content_hashes: vec!["abc".into()],
        original_blob_sha: None,
        prompt: prompt.into(),
        conversation_id: "sess_x".into(),
        turn_index: 0,
        tool_use_id: None,
        preceding_turns_summary: None,
        model: "claude-sonnet-4-5".into(),
        tool: "claude-code".into(),
        timestamp: ts.into(),
        derived_from: None,
    }
}

fn make_commit(root: &Path, content: &str, msg: &str) -> String {
    std::fs::write(root.join("README.md"), content).unwrap();
    run_git(root, &["add", "README.md"]);
    run_git(root, &["commit", "-q", "-m", msg]);
    head_sha(root)
}

// ---- prov repair ----

#[test]
fn repair_reattaches_orphan_after_amend_bypassed_hook() {
    let tmp = init_repo();
    let root = tmp.path();
    let old = make_commit(root, "v1\n", "first");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    store
        .write(
            &old,
            &Note::new(vec![make_edit("orphan prompt", "2026-04-28T12:00:00Z")]),
        )
        .unwrap();

    // Amend without running the post-rewrite hook (simulating bypass).
    run_git(
        root,
        &[
            "commit",
            "--amend",
            "-q",
            "-m",
            "first amended",
            "--allow-empty",
        ],
    );
    let new = head_sha(root);
    assert!(store.read(&new).unwrap().is_none());
    assert!(store.read(&old).unwrap().is_some());

    prov_in(root).args(["repair"]).assert().success();

    assert!(
        store.read(&new).unwrap().is_some(),
        "note migrated to new SHA"
    );
    assert!(store.read(&old).unwrap().is_none(), "old orphan removed");
}

#[test]
fn repair_is_noop_when_no_orphans() {
    let tmp = init_repo();
    let root = tmp.path();
    let _head = make_commit(root, "v1\n", "first");

    prov_in(root)
        .args(["repair"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no rewrite events"));
}

#[test]
fn repair_dry_run_does_not_write() {
    let tmp = init_repo();
    let root = tmp.path();
    let old = make_commit(root, "v1\n", "first");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store
        .write(
            &old,
            &Note::new(vec![make_edit("p", "2026-04-28T12:00:00Z")]),
        )
        .unwrap();

    run_git(
        root,
        &["commit", "--amend", "-q", "-m", "amended", "--allow-empty"],
    );
    let new = head_sha(root);

    prov_in(root)
        .args(["repair", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("would migrate"));

    // Old note still present, new SHA still empty.
    assert!(store.read(&old).unwrap().is_some());
    assert!(store.read(&new).unwrap().is_none());
}

#[test]
fn repair_skips_when_new_sha_already_has_note() {
    let tmp = init_repo();
    let root = tmp.path();
    let old = make_commit(root, "v1\n", "first");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store
        .write(
            &old,
            &Note::new(vec![make_edit("orphan", "2026-04-28T12:00:00Z")]),
        )
        .unwrap();

    run_git(
        root,
        &["commit", "--amend", "-q", "-m", "amended", "--allow-empty"],
    );
    let new = head_sha(root);
    // Pre-stage a note on the new SHA — repair must not clobber it.
    store
        .write(
            &new,
            &Note::new(vec![make_edit("already there", "2026-04-29T00:00:00Z")]),
        )
        .unwrap();

    prov_in(root).args(["repair"]).assert().success();

    let on_new = store.read(&new).unwrap().unwrap();
    assert_eq!(on_new.edits[0].prompt, "already there");
    // Old SHA's orphan is still there because repair declined to touch the
    // pre-existing new-SHA note.
    assert!(store.read(&old).unwrap().is_some());
}

// ---- prov gc ----

#[test]
fn gc_culls_unreachable_notes() {
    let tmp = init_repo();
    let root = tmp.path();
    let _head = make_commit(root, "v1\n", "kept");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    // Attach a note to a SHA that does not exist (treated as unreachable —
    // git also treats nonexistent objects as not-reachable from any ref).
    let phantom = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    // Need a real object to write a note to — fabricate one by using an
    // existing tree's commit. Easier: write a note to the kept HEAD and a
    // separate one we'll orphan by removing the only ref reaching it.
    let _ = phantom;

    // Make a second commit on a branch, attach a note, then delete the branch.
    run_git(root, &["checkout", "-q", "-b", "scratch"]);
    let unreachable_sha = make_commit(root, "scratch\n", "scratch");
    store
        .write(
            &unreachable_sha,
            &Note::new(vec![make_edit("dead prompt", "2026-04-28T12:00:00Z")]),
        )
        .unwrap();
    run_git(root, &["checkout", "-q", "main"]);
    run_git(root, &["branch", "-q", "-D", "scratch"]);

    // Sanity: git for-each-ref --contains should be empty for unreachable.
    let out = Command::new("git")
        .current_dir(root)
        .args(["for-each-ref", "--contains", &unreachable_sha])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8(out.stdout).unwrap().trim().is_empty(),
        "scratch branch should be unreachable"
    );

    prov_in(root).args(["gc"]).assert().success();

    assert!(store.read(&unreachable_sha).unwrap().is_none());
}

#[test]
fn gc_dry_run_reports_without_writing() {
    let tmp = init_repo();
    let root = tmp.path();
    let _head = make_commit(root, "v1\n", "kept");
    run_git(root, &["checkout", "-q", "-b", "scratch"]);
    let unreachable_sha = make_commit(root, "scratch\n", "scratch");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store
        .write(
            &unreachable_sha,
            &Note::new(vec![make_edit("dead", "2026-04-28T12:00:00Z")]),
        )
        .unwrap();

    run_git(root, &["checkout", "-q", "main"]);
    run_git(root, &["branch", "-q", "-D", "scratch"]);

    prov_in(root)
        .args(["gc", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("would cull"));

    // Note still present.
    assert!(store.read(&unreachable_sha).unwrap().is_some());
}

#[test]
fn gc_prunes_stale_staging_sessions() {
    use std::time::{Duration, SystemTime};

    let tmp = init_repo();
    let root = tmp.path();
    let _head = make_commit(root, "v1\n", "init");

    let git = Git::discover(root).unwrap();
    let staging = Staging::new(git.git_dir());
    let sid = prov_core::session::SessionId::parse("stale_sess").unwrap();
    staging.ensure_session_dir(&sid, false).unwrap();
    let session_dir = staging.session_dir(&sid, false);
    let marker = session_dir.join("turn-0.json");
    std::fs::write(&marker, "{}").unwrap();

    // Backdate via std::fs::File::set_modified (stable since 1.75 — matches
    // workspace MSRV). Avoids pulling in `filetime` for one test setup.
    let thirty_days_ago = SystemTime::now() - Duration::from_secs(30 * 86_400);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&marker)
        .unwrap();
    f.set_modified(thirty_days_ago).unwrap();

    prov_in(root)
        .args(["gc", "--staging-ttl-days", "14"])
        .assert()
        .success();

    assert!(!session_dir.exists(), "stale session pruned");
}

#[test]
fn gc_compact_drops_summary_for_old_notes() {
    let tmp = init_repo();
    let root = tmp.path();
    let head = make_commit(root, "v1\n", "init");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let mut edit = make_edit("old", "2020-01-01T00:00:00Z");
    edit.preceding_turns_summary = Some("a long historical summary".into());
    edit.original_blob_sha = Some("0000000000000000000000000000000000000000".into());
    store.write(&head, &Note::new(vec![edit])).unwrap();

    prov_in(root).args(["gc", "--compact"]).assert().success();

    let after = store.read(&head).unwrap().unwrap();
    assert!(after.edits[0].preceding_turns_summary.is_none());
    // Bogus blob is unreachable → cleared.
    assert!(after.edits[0].original_blob_sha.is_none());
}

#[test]
fn gc_compact_skips_recent_notes() {
    let tmp = init_repo();
    let root = tmp.path();
    let head = make_commit(root, "v1\n", "init");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let mut edit = make_edit("recent", "2030-01-01T00:00:00Z");
    edit.preceding_turns_summary = Some("keep me".into());
    store.write(&head, &Note::new(vec![edit])).unwrap();

    prov_in(root).args(["gc", "--compact"]).assert().success();

    let after = store.read(&head).unwrap().unwrap();
    assert_eq!(
        after.edits[0].preceding_turns_summary.as_deref(),
        Some("keep me")
    );
}

#[test]
fn gc_compact_skips_notes_with_empty_timestamps() {
    // Notes whose edits all carry empty timestamps must not be compacted —
    // there's no way to compare them against the cutoff. Older versions
    // would have routed them through compaction anyway because the empty
    // string compares lexicographically below any real ISO date.
    let tmp = init_repo();
    let root = tmp.path();
    let head = make_commit(root, "v1\n", "init");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let mut edit = make_edit("no timestamp", "");
    edit.preceding_turns_summary = Some("untouched".into());
    store.write(&head, &Note::new(vec![edit])).unwrap();

    prov_in(root).args(["gc", "--compact"]).assert().success();

    let after = store.read(&head).unwrap().unwrap();
    assert_eq!(
        after.edits[0].preceding_turns_summary.as_deref(),
        Some("untouched")
    );
}

#[test]
fn gc_preserves_notes_for_detached_head_commits() {
    // Regression: `prov gc` previously called `for-each-ref --contains <sha>`
    // alone, which returns empty for commits reachable only via detached HEAD.
    // The user's WIP debug commit's note would be silently culled.
    let tmp = init_repo();
    let root = tmp.path();
    let _seed = make_commit(root, "seed\n", "seed");

    // Create a commit, then detach HEAD to it (no branch points at it).
    let detached_sha = make_commit(root, "wip\n", "wip on detached HEAD");
    run_git(root, &["checkout", "-q", "--detach", &detached_sha]);
    // Move main back so the only path to detached_sha is via reflog/HEAD.
    run_git(root, &["update-ref", "refs/heads/main", "HEAD~"]);

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store
        .write(
            &detached_sha,
            &Note::new(vec![make_edit("wip prompt", "2026-04-28T12:00:00Z")]),
        )
        .unwrap();

    prov_in(root).args(["gc"]).assert().success();

    assert!(
        store.read(&detached_sha).unwrap().is_some(),
        "detached-HEAD note was culled — reflog reachability fallback failed"
    );
}
