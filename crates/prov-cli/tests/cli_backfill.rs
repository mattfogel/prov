//! End-to-end tests for `prov backfill` (U15).
//!
//! Each test sets up a fresh fixture repo with one or more commits, drops a
//! synthetic Claude Code transcript JSONL on disk, and drives `prov backfill
//! --transcript-path <file> --yes` against it. Assertions cover the full
//! match → redact → note-write pipeline, idempotency, and the safety
//! interlocks (cross-author refusal, live-note non-overwrite, low-confidence
//! suppression).

use std::path::Path;
use std::process::Command;

use assert_cmd::Command as AssertCommand;
use predicates::prelude::*;
use tempfile::TempDir;

use prov_core::git::Git;
use prov_core::schema::{DerivedFrom, Edit, Note};
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

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
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

/// Commit a single file with the given content. Returns the commit SHA.
fn commit_file(repo: &Path, name: &str, contents: &str, msg: &str) -> String {
    commit_file_at(repo, name, contents, msg, "2026-04-28T12:30:00+0000")
}

/// Commit with explicit author/committer timestamp so the test can place the
/// commit inside backfill's 4-hour grace window relative to a synthetic
/// transcript timestamp.
fn commit_file_at(repo: &Path, name: &str, contents: &str, msg: &str, when: &str) -> String {
    if let Some(parent) = Path::new(name).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(repo.join(parent)).unwrap();
        }
    }
    std::fs::write(repo.join(name), contents).unwrap();
    let status = Command::new("git")
        .current_dir(repo)
        .args(["add", name])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git add");
    assert!(status.success());
    let status = Command::new("git")
        .current_dir(repo)
        .args(["commit", "-q", "-m", msg])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_DATE", when)
        .env("GIT_COMMITTER_DATE", when)
        .status()
        .expect("git commit");
    assert!(status.success());
    git_stdout(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

/// Write a synthetic transcript whose Edit/Write events reference `file_abs`.
/// The transcript is intentionally simple — one user turn, one tool_use —
/// so the matcher's behavior is easy to reason about.
fn write_transcript(
    transcript_dir: &Path,
    session_id: &str,
    timestamp: &str,
    cwd: &str,
    file_abs: &str,
    new_string: &str,
    prompt: &str,
) -> std::path::PathBuf {
    write_transcript_with_tool(
        transcript_dir,
        session_id,
        timestamp,
        cwd,
        file_abs,
        "Write",
        new_string,
        prompt,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_transcript_with_tool(
    transcript_dir: &Path,
    session_id: &str,
    timestamp: &str,
    cwd: &str,
    file_abs: &str,
    tool: &str,
    new_string: &str,
    prompt: &str,
) -> std::path::PathBuf {
    let path = transcript_dir.join(format!("{session_id}.jsonl"));
    let prompt_obj = serde_json::json!({
        "type": "user",
        "sessionId": session_id,
        "promptId": format!("p-{session_id}"),
        "timestamp": timestamp,
        "cwd": cwd,
        "message": { "role": "user", "content": prompt },
    });
    let input = match tool {
        "Write" => serde_json::json!({"file_path": file_abs, "content": new_string}),
        "Edit" => serde_json::json!({
            "file_path": file_abs,
            "old_string": "",
            "new_string": new_string,
        }),
        _ => panic!("unsupported tool in fixture: {tool}"),
    };
    let assistant_obj = serde_json::json!({
        "type": "assistant",
        "sessionId": session_id,
        "timestamp": timestamp,
        "message": {
            "model": "claude-sonnet-4-7",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": format!("toolu-{session_id}"),
                "name": tool,
                "input": input,
            }],
        },
    });
    let body = format!(
        "{}\n{}\n",
        serde_json::to_string(&prompt_obj).unwrap(),
        serde_json::to_string(&assistant_obj).unwrap()
    );
    std::fs::write(&path, body).unwrap();
    path
}

// ============================================================
// Happy path
// ============================================================

#[test]
fn writes_backfill_note_for_matching_commit() {
    let tmp = init_repo();
    let file_abs = tmp.path().join("src/lib.rs").to_string_lossy().into_owned();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    let contents = "fn one() {\n    println!(\"hi\");\n}\n";
    let sha = commit_file(tmp.path(), "src/lib.rs", contents, "feat: add lib");

    let transcripts = TempDir::new().unwrap();
    let transcript_path = write_transcript(
        transcripts.path(),
        "sess-happy",
        "2026-04-28T12:00:00Z",
        &tmp.path().to_string_lossy(),
        &file_abs,
        contents,
        "add a one() function with a hi message",
    );

    prov_in(tmp.path())
        .args([
            "backfill",
            "--yes",
            "--transcript-path",
            &transcript_path.to_string_lossy(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("backfilled"));

    // Verify the note exists, is marked Backfill, and carries the redacted
    // prompt + matched line range.
    let git = Git::discover(tmp.path()).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let note = store
        .read(&sha)
        .unwrap()
        .expect("backfill should have written a note");
    assert_eq!(note.edits.len(), 1);
    let edit = &note.edits[0];
    assert!(matches!(
        edit.derived_from,
        Some(DerivedFrom::Backfill { .. })
    ));
    if let Some(DerivedFrom::Backfill { confidence, .. }) = &edit.derived_from {
        assert!(*confidence >= 0.6);
    }
    assert_eq!(edit.file, "src/lib.rs");
    assert_eq!(edit.prompt, "add a one() function with a hi message");
    assert_eq!(edit.tool, "claude-code");
    assert_eq!(edit.conversation_id, "sess-happy");
    // Hashes recorded — required so the resolver can do drift detection on
    // backfilled lines the same way it does for live captures.
    assert!(!edit.content_hashes.is_empty());
}

#[test]
fn redacts_secrets_in_backfilled_prompts() {
    let tmp = init_repo();
    let file_abs = tmp.path().join("README.md").to_string_lossy().into_owned();
    let contents = "hello\nfrom\nprov\n";
    let _sha = commit_file(tmp.path(), "README.md", contents, "docs: README");

    let transcripts = TempDir::new().unwrap();
    let secret_prompt = "use AKIAIOSFODNN7EXAMPLE for the AWS demo";
    let transcript_path = write_transcript(
        transcripts.path(),
        "sess-secret",
        "2026-04-28T12:00:00Z",
        &tmp.path().to_string_lossy(),
        &file_abs,
        contents,
        secret_prompt,
    );

    prov_in(tmp.path())
        .args([
            "backfill",
            "--yes",
            "--transcript-path",
            &transcript_path.to_string_lossy(),
        ])
        .assert()
        .success();

    let git = Git::discover(tmp.path()).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let head = git_stdout(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let note = store.read(&head).unwrap().expect("note");
    let edit = &note.edits[0];
    assert!(
        !edit.prompt.contains("AKIAIOSFODNN7EXAMPLE"),
        "AWS key should have been redacted; prompt was: {}",
        edit.prompt
    );
    assert!(
        edit.prompt.contains("[REDACTED:"),
        "redactor should have stamped a marker; prompt was: {}",
        edit.prompt
    );
}

// ============================================================
// Idempotency
// ============================================================

#[test]
fn rerun_replaces_existing_backfill_note_without_duplicates() {
    let tmp = init_repo();
    let file_abs = tmp.path().join("a.txt").to_string_lossy().into_owned();
    let contents = "alpha\nbeta\ngamma\n";
    let sha = commit_file(tmp.path(), "a.txt", contents, "chore: a");

    let transcripts = TempDir::new().unwrap();
    let transcript_path = write_transcript(
        transcripts.path(),
        "sess-idem",
        "2026-04-28T12:00:00Z",
        &tmp.path().to_string_lossy(),
        &file_abs,
        contents,
        "create alpha beta gamma",
    );

    for _ in 0..2 {
        prov_in(tmp.path())
            .args([
                "backfill",
                "--yes",
                "--transcript-path",
                &transcript_path.to_string_lossy(),
            ])
            .assert()
            .success();
    }

    let git = Git::discover(tmp.path()).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let note = store.read(&sha).unwrap().expect("note");
    assert_eq!(note.edits.len(), 1, "re-run must not duplicate edits");
}

#[test]
fn refuses_to_overwrite_live_captured_note() {
    let tmp = init_repo();
    let file_abs = tmp.path().join("b.txt").to_string_lossy().into_owned();
    let contents = "delta\nepsilon\n";
    let sha = commit_file(tmp.path(), "b.txt", contents, "chore: b");

    // Pre-seed a live (not derived_from-Backfill) note on this commit.
    let git = Git::discover(tmp.path()).unwrap();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);
    let live_edit = Edit {
        file: "b.txt".into(),
        line_range: [1, 2],
        content_hashes: vec!["h1".into(), "h2".into()],
        original_blob_sha: None,
        prompt: "hand-written prompt".into(),
        conversation_id: "live-session".into(),
        turn_index: 0,
        tool_use_id: None,
        preceding_turns_summary: None,
        model: "claude-opus-4-7".into(),
        tool: "claude-code".into(),
        timestamp: "2026-04-28T12:00:00Z".into(),
        derived_from: None,
    };
    store.write(&sha, &Note::new(vec![live_edit])).unwrap();

    let transcripts = TempDir::new().unwrap();
    let transcript_path = write_transcript(
        transcripts.path(),
        "sess-clobber",
        "2026-04-28T12:00:00Z",
        &tmp.path().to_string_lossy(),
        &file_abs,
        contents,
        "would overwrite",
    );

    prov_in(tmp.path())
        .args([
            "backfill",
            "--yes",
            "--transcript-path",
            &transcript_path.to_string_lossy(),
        ])
        .assert()
        .success();

    let note = store.read(&sha).unwrap().expect("live note");
    assert_eq!(note.edits[0].prompt, "hand-written prompt");
    assert!(note.edits[0].derived_from.is_none());
}

// ============================================================
// No-match path
// ============================================================

#[test]
fn no_matching_commits_exits_zero_with_zero_summary() {
    let tmp = init_repo();
    let _sha = commit_file(tmp.path(), "real.txt", "real-line\n", "init");

    let transcripts = TempDir::new().unwrap();
    let transcript_path = write_transcript(
        transcripts.path(),
        "sess-nomatch",
        "2026-04-28T12:00:00Z",
        &tmp.path().to_string_lossy(),
        // Reference a file that does not exist in the repo so nothing matches.
        &tmp.path().join("ghost.txt").to_string_lossy(),
        "ghost-content-line\n",
        "ghost prompt",
    );

    prov_in(tmp.path())
        .args([
            "backfill",
            "--yes",
            "--transcript-path",
            &transcript_path.to_string_lossy(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 note(s) written"));
}

// ============================================================
// Author check
// ============================================================

#[test]
fn cross_author_commit_skipped_without_flag() {
    let tmp = init_repo();
    // Commit authored by a *different* email than user.email and timestamped
    // inside the synthetic transcript's grace window.
    std::fs::write(tmp.path().join("c.txt"), "hello\n").unwrap();
    run_git(tmp.path(), &["add", "c.txt"]);
    let status = Command::new("git")
        .current_dir(tmp.path())
        .args(["commit", "-q", "-m", "by other"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Other")
        .env("GIT_AUTHOR_EMAIL", "other@x.com")
        .env("GIT_COMMITTER_NAME", "Other")
        .env("GIT_COMMITTER_EMAIL", "other@x.com")
        .env("GIT_AUTHOR_DATE", "2026-04-28T12:30:00+0000")
        .env("GIT_COMMITTER_DATE", "2026-04-28T12:30:00+0000")
        .status()
        .unwrap();
    assert!(status.success());
    let sha = git_stdout(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();

    let transcripts = TempDir::new().unwrap();
    let file_abs = tmp.path().join("c.txt").to_string_lossy().into_owned();
    let transcript_path = write_transcript(
        transcripts.path(),
        "sess-cross",
        "2026-04-28T12:00:00Z",
        &tmp.path().to_string_lossy(),
        &file_abs,
        "hello\n",
        "say hello",
    );

    // Without --cross-author: skip the commit (no note).
    prov_in(tmp.path())
        .args([
            "backfill",
            "--yes",
            "--transcript-path",
            &transcript_path.to_string_lossy(),
        ])
        .assert()
        .success();

    let git = Git::discover(tmp.path()).unwrap();
    let store = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    assert!(store.read(&sha).unwrap().is_none());

    // With --cross-author: note is written.
    prov_in(tmp.path())
        .args([
            "backfill",
            "--yes",
            "--cross-author",
            "--transcript-path",
            &transcript_path.to_string_lossy(),
        ])
        .assert()
        .success();
    assert!(store.read(&sha).unwrap().is_some());
}

// ============================================================
// Smoke: non-existent transcript path errors out cleanly
// ============================================================

#[test]
fn missing_transcript_path_errors() {
    let tmp = init_repo();
    let _ = commit_file(tmp.path(), "x.txt", "x\n", "init");
    prov_in(tmp.path())
        .args([
            "backfill",
            "--yes",
            "--transcript-path",
            "/no/such/path/transcript.jsonl",
        ])
        .assert()
        .failure();
}
