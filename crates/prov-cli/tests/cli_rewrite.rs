//! End-to-end tests for U9 — post-rewrite handler (amend / rebase / squash)
//! and cherry-pick (regression coverage; cherry-pick lives in U3's
//! post-commit handler, but the test belongs here because it's part of the
//! "notes survive rewrites" R2 coverage U9 closes out).
//!
//! These drive `prov hook post-rewrite` directly with the documented
//! `<old-sha> <new-sha>` stdin format. Going through `git commit --amend`
//! / `git rebase` would also work but is much slower; the handler's contract
//! with git is the stdin format, so testing that is sufficient.

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use tempfile::TempDir;

use prov_core::git::Git;
use prov_core::schema::{Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

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

fn make_edit(
    prompt: &str,
    conversation_id: &str,
    turn_index: u32,
    tool_use_id: Option<&str>,
) -> Edit {
    Edit {
        file: "README.md".into(),
        line_range: [1, 1],
        content_hashes: vec!["abc".into()],
        original_blob_sha: None,
        prompt: prompt.into(),
        conversation_id: conversation_id.into(),
        turn_index,
        tool_use_id: tool_use_id.map(String::from),
        preceding_turns_summary: None,
        model: "claude-sonnet-4-5".into(),
        tool: "claude-code".into(),
        timestamp: format!("2026-04-28T12:00:{turn_index:02}Z"),
        derived_from: None,
    }
}

fn make_commit(root: &Path, content: &str, msg: &str) -> String {
    std::fs::write(root.join("README.md"), content).unwrap();
    run_git(root, &["add", "README.md"]);
    run_git(root, &["commit", "-q", "-m", msg]);
    head_sha(root)
}

#[test]
fn amend_one_to_one_preserves_note() {
    let tmp = init_repo();
    let root = tmp.path();
    let old = make_commit(root, "v1\n", "first");

    // Stage a public note on `old`.
    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    store
        .write(
            &old,
            &Note::new(vec![make_edit(
                "write a readme",
                "sess_a",
                0,
                Some("toolu_1"),
            )]),
        )
        .unwrap();

    // Amend → new SHA.
    run_git(
        root,
        &[
            "commit",
            "--amend",
            "-q",
            "-m",
            "first (amended)",
            "--allow-empty",
        ],
    );
    let new = head_sha(root);
    assert_ne!(old, new);

    // Drive the handler with a 1:1 mapping.
    prov_in(root)
        .args(["hook", "post-rewrite", "amend"])
        .write_stdin(format!("{old} {new}\n"))
        .assert()
        .success();

    let on_new = store.read(&new).unwrap().expect("note migrated to new SHA");
    assert_eq!(on_new.edits[0].prompt, "write a readme");
    assert!(store.read(&old).unwrap().is_none(), "old note removed");
}

#[test]
fn rebase_reorder_preserves_each_note() {
    let tmp = init_repo();
    let root = tmp.path();
    let a = make_commit(root, "a\n", "A");
    let b = make_commit(root, "ab\n", "B");
    let c = make_commit(root, "abc\n", "C");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    store
        .write(
            &a,
            &Note::new(vec![make_edit("A's prompt", "sess_a", 0, Some("toolu_a"))]),
        )
        .unwrap();
    store
        .write(
            &b,
            &Note::new(vec![make_edit("B's prompt", "sess_b", 0, Some("toolu_b"))]),
        )
        .unwrap();
    store
        .write(
            &c,
            &Note::new(vec![make_edit("C's prompt", "sess_c", 0, Some("toolu_c"))]),
        )
        .unwrap();

    // Simulate rebase that produced 3 new SHAs (we don't care if they look
    // like a real rebase — the handler only sees the pairs).
    let new_a = "1111111111111111111111111111111111111111";
    let new_b = "2222222222222222222222222222222222222222";
    let new_c = "3333333333333333333333333333333333333333";
    let stdin = format!("{a} {new_a}\n{b} {new_b}\n{c} {new_c}\n");
    prov_in(root)
        .args(["hook", "post-rewrite", "rebase"])
        .write_stdin(stdin)
        .assert()
        .success();

    // Each note migrated.
    assert_eq!(
        store.read(new_a).unwrap().unwrap().edits[0].prompt,
        "A's prompt"
    );
    assert_eq!(
        store.read(new_b).unwrap().unwrap().edits[0].prompt,
        "B's prompt"
    );
    assert_eq!(
        store.read(new_c).unwrap().unwrap().edits[0].prompt,
        "C's prompt"
    );
    // Old notes removed.
    assert!(store.read(&a).unwrap().is_none());
    assert!(store.read(&b).unwrap().is_none());
    assert!(store.read(&c).unwrap().is_none());
}

#[test]
fn squash_n_to_one_merges_edits_deduped_and_sorted() {
    let tmp = init_repo();
    let root = tmp.path();
    let a = make_commit(root, "a\n", "A");
    let b = make_commit(root, "ab\n", "B");
    let c = make_commit(root, "abc\n", "C");

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    // Three notes, each with one distinct edit. Plus a deliberate duplicate
    // on B with the same `(conversation_id, turn_index, tool_use_id)` as A's
    // edit but a *later* timestamp — dedupe should keep the later one.
    let edit_a = make_edit("A's prompt", "sess_x", 0, Some("toolu_x"));
    let mut edit_dup = make_edit("A re-staged later", "sess_x", 0, Some("toolu_x"));
    edit_dup.timestamp = "2026-04-28T13:00:00Z".into();
    store.write(&a, &Note::new(vec![edit_a])).unwrap();
    store
        .write(
            &b,
            &Note::new(vec![
                edit_dup,
                make_edit("B's prompt", "sess_y", 0, Some("toolu_y")),
            ]),
        )
        .unwrap();
    store
        .write(
            &c,
            &Note::new(vec![make_edit("C's prompt", "sess_z", 0, Some("toolu_z"))]),
        )
        .unwrap();

    // All three squash into the same new SHA.
    let new = "abcdef0000000000000000000000000000000001";
    let stdin = format!("{a} {new}\n{b} {new}\n{c} {new}\n");
    prov_in(root)
        .args(["hook", "post-rewrite", "rebase"])
        .write_stdin(stdin)
        .assert()
        .success();

    let merged = store.read(new).unwrap().expect("squashed note present");
    // Three distinct dedupe keys → three edits in the merged note.
    let prompts: Vec<&str> = merged.edits.iter().map(|e| e.prompt.as_str()).collect();
    assert_eq!(merged.edits.len(), 3, "got: {prompts:?}");
    // The duplicate-by-key collapsed to the *later* timestamp's prompt.
    assert!(prompts.contains(&"A re-staged later"));
    assert!(!prompts.contains(&"A's prompt"));
    // Sorted ascending by timestamp.
    let timestamps: Vec<&str> = merged.edits.iter().map(|e| e.timestamp.as_str()).collect();
    let mut sorted = timestamps.clone();
    sorted.sort_unstable();
    assert_eq!(timestamps, sorted);
}

#[test]
fn rewrite_skips_old_with_no_note() {
    let tmp = init_repo();
    let root = tmp.path();
    let no_note = make_commit(root, "v1\n", "no note");

    let new = "feedface00000000000000000000000000000000";
    prov_in(root)
        .args(["hook", "post-rewrite", "amend"])
        .write_stdin(format!("{no_note} {new}\n"))
        .assert()
        .success();

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    assert!(store.read(new).unwrap().is_none());
}

#[test]
fn rewrite_migrates_private_ref_too() {
    let tmp = init_repo();
    let root = tmp.path();
    let old = make_commit(root, "v1\n", "first");
    let git = Git::discover(root).unwrap();
    let private = NotesStore::new(git, NOTES_REF_PRIVATE);
    private
        .write(
            &old,
            &Note::new(vec![make_edit("private secret", "sess_p", 0, None)]),
        )
        .unwrap();

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

    prov_in(root)
        .args(["hook", "post-rewrite", "amend"])
        .write_stdin(format!("{old} {new}\n"))
        .assert()
        .success();

    let on_new = private.read(&new).unwrap().expect("private note migrated");
    assert_eq!(on_new.edits[0].prompt, "private secret");
    assert!(private.read(&old).unwrap().is_none());
}

#[test]
fn rewrite_handles_existing_note_on_new_sha_by_merging() {
    let tmp = init_repo();
    let root = tmp.path();
    let old = make_commit(root, "v1\n", "first");
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

    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    // Pre-seed a note on the new SHA (e.g., post-commit ran before
    // post-rewrite finished migrating).
    store
        .write(
            &new,
            &Note::new(vec![make_edit(
                "captured fresh",
                "sess_q",
                0,
                Some("toolu_q"),
            )]),
        )
        .unwrap();
    // And a note on old that needs to migrate in.
    store
        .write(
            &old,
            &Note::new(vec![make_edit(
                "captured before amend",
                "sess_p",
                0,
                Some("toolu_p"),
            )]),
        )
        .unwrap();

    prov_in(root)
        .args(["hook", "post-rewrite", "amend"])
        .write_stdin(format!("{old} {new}\n"))
        .assert()
        .success();

    let merged = store.read(&new).unwrap().expect("merged note");
    assert_eq!(merged.edits.len(), 2);
    let prompts: Vec<&str> = merged.edits.iter().map(|e| e.prompt.as_str()).collect();
    assert!(prompts.contains(&"captured fresh"));
    assert!(prompts.contains(&"captured before amend"));
}

#[test]
fn rewrite_skips_invalid_stdin_lines_defensively() {
    let tmp = init_repo();
    let root = tmp.path();
    let old = make_commit(root, "v1\n", "first");
    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    store
        .write(&old, &Note::new(vec![make_edit("p", "sess_x", 0, None)]))
        .unwrap();

    // Garbage interspersed with a valid pair must not break the handler.
    let new = "abcabcabcabcabcabcabcabcabcabcabcabcabca";
    let stdin = format!("garbage line\nshort sha\n{old} {new}\nnotenough\n");
    prov_in(root)
        .args(["hook", "post-rewrite", "rebase"])
        .write_stdin(stdin)
        .assert()
        .success();

    assert!(store.read(new).unwrap().is_some());
}

#[test]
fn cherry_pick_stamps_derived_from_via_post_commit() {
    // Regression coverage for U3's cherry-pick path. The post-commit handler
    // stamps `derived_from: Rewrite` on the cherry-picked edit so the source
    // commit is still recoverable.
    use prov_core::schema::DerivedFrom;
    use prov_core::storage::staging::Staging;

    let tmp = init_repo();
    let root = tmp.path();
    // Make a commit and a note on it so cherry-pick has something to derive
    // from. The actual `derived_from` stamping happens via the staged-edit
    // path in handle_post_commit, so we set up a staging entry that matches.
    let source = make_commit(root, "src content\n", "source");
    let git = Git::discover(root).unwrap();
    let store = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    store
        .write(
            &source,
            &Note::new(vec![make_edit(
                "source prompt",
                "sess_s",
                0,
                Some("toolu_s"),
            )]),
        )
        .unwrap();

    // Stage an edit whose `after` content matches what the cherry-pick will
    // re-add to a fresh branch.
    let staging = Staging::new(git.git_dir());
    let sid = prov_core::session::SessionId::parse("sess_cp").unwrap();
    staging
        .write_session_meta(
            &sid,
            &prov_core::storage::staging::SessionMeta {
                session_id: "sess_cp".into(),
                model: "claude-sonnet-4-5".into(),
                started_at: "2026-04-28T12:00:00Z".into(),
            },
        )
        .unwrap();
    staging
        .write_turn(
            &sid,
            false,
            0,
            &prov_core::storage::staging::TurnRecord {
                session_id: "sess_cp".into(),
                turn_index: 0,
                prompt: "cherry-picked prompt".into(),
                private: false,
                transcript_path: None,
                cwd: None,
                started_at: "2026-04-28T12:00:00Z".into(),
                completed_at: None,
            },
        )
        .unwrap();
    staging
        .append_edit(
            &sid,
            false,
            &prov_core::storage::staging::EditRecord {
                session_id: "sess_cp".into(),
                turn_index: 0,
                tool_use_id: Some("toolu_cp".into()),
                tool_name: "Write".into(),
                file: "PICKED.md".into(),
                line_range: [1, 1],
                before: String::new(),
                after: "picked\n".into(),
                content_hashes: vec![blake3::hash(b"picked").to_hex().to_string()],
                timestamp: "2026-04-28T12:00:00Z".into(),
            },
        )
        .unwrap();

    // Branch off, simulate cherry-pick by writing CHERRY_PICK_HEAD then
    // committing matching content. The handler reads CHERRY_PICK_HEAD before
    // matching edits.
    run_git(root, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(root.join("PICKED.md"), "picked\n").unwrap();
    run_git(root, &["add", "PICKED.md"]);
    run_git(root, &["commit", "-q", "-m", "cherry"]);
    let new = head_sha(root);
    // Simulate the in-progress cherry-pick state: real `git cherry-pick`
    // leaves CHERRY_PICK_HEAD in place across the commit so post-commit can
    // read it. We're driving the hook manually here, so write the marker
    // before invoking and clean up after.
    std::fs::write(git.git_dir().join("CHERRY_PICK_HEAD"), &source).unwrap();

    prov_in(root)
        .args(["hook", "post-commit"])
        .write_stdin("")
        .assert()
        .success();
    let _ = std::fs::remove_file(git.git_dir().join("CHERRY_PICK_HEAD"));

    let on_new = store.read(&new).unwrap().expect("cherry-pick note present");
    let derived = on_new.edits[0]
        .derived_from
        .as_ref()
        .expect("derived_from set");
    match derived {
        DerivedFrom::Rewrite { source_commit, .. } => {
            assert_eq!(source_commit, &source);
        }
        other => panic!("expected DerivedFrom::Rewrite, got {other:?}"),
    }
}
