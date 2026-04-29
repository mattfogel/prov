//! End-to-end storage tests using a real fixture git repo.
//!
//! Covers the round-trip path NotesStore -> Cache::reindex_from -> queries.
//! Unit-level coverage for schema/git/cache lives next to those modules; this
//! file proves they compose correctly.

use prov_core::git::Git;
use prov_core::schema::{Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::NOTES_REF_PUBLIC;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Initialize a fresh git repo with an initial commit and return (tempdir, Git, head_sha).
fn fixture_repo() -> (TempDir, Git, String) {
    let dir = TempDir::new().unwrap();
    git_in(dir.path(), &["init", "-q", "-b", "main", "."]);
    git_in(dir.path(), &["config", "--local", "user.email", "t@x.com"]);
    git_in(dir.path(), &["config", "--local", "user.name", "T"]);
    std::fs::write(dir.path().join("README.md"), "hello").unwrap();
    git_in(dir.path(), &["add", "README.md"]);
    git_in(dir.path(), &["commit", "-q", "-m", "initial"]);
    let head = git_capture(dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let git = Git::discover(dir.path()).unwrap();
    (dir, git, head)
}

fn git_in(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn git_capture(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

fn make_edit(file: &str, prompt: &str, line_start: u32, line_end: u32) -> Edit {
    let n = (line_end - line_start + 1) as usize;
    Edit {
        file: file.into(),
        line_range: [line_start, line_end],
        content_hashes: (0..n).map(|i| format!("h{i}")).collect(),
        original_blob_sha: "blob_sha".into(),
        prompt: prompt.into(),
        conversation_id: "sess_test".into(),
        turn_index: 0,
        tool_use_id: None,
        preceding_turns_summary: String::new(),
        model: "claude-sonnet-4-5".into(),
        tool: "claude-code".into(),
        timestamp: "2026-04-28T12:00:00Z".into(),
        derived_from: None,
    }
}

#[test]
fn write_via_store_reindex_into_cache_roundtrip() {
    let (_dir, git, sha) = fixture_repo();
    let store = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);

    let note = Note::new(vec![
        make_edit("src/auth.ts", "make this faster", 10, 20),
        make_edit("src/payments.ts", "use 24h dedupe window", 50, 55),
    ]);
    store.write(&sha, &note).unwrap();

    let mut cache = Cache::open_in_memory().unwrap();
    let stats = cache.reindex_from(&store).unwrap();
    assert_eq!(stats.notes, 1);
    assert_eq!(stats.edits, 2);

    // recorded notes-ref SHA should match the live ref.
    let live_ref = store.ref_sha().unwrap().expect("ref exists post-write");
    let recorded = cache.recorded_notes_ref_sha().unwrap().expect("recorded");
    assert_eq!(live_ref, recorded);

    // Note round-trip via cache.
    let cached = cache.get_note(&sha).unwrap().expect("cached");
    assert_eq!(cached, note);

    // Per-file query.
    let auth_edits = cache.edits_for_file("src/auth.ts").unwrap();
    assert_eq!(auth_edits.len(), 1);
    assert_eq!(auth_edits[0].prompt, "make this faster");
    assert_eq!(auth_edits[0].line_start, 10);
    assert_eq!(auth_edits[0].line_end, 20);
}

#[test]
fn fts_search_finds_prompt_substrings() {
    let (_dir, git, sha) = fixture_repo();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);

    let note = Note::new(vec![
        make_edit("src/a.ts", "implement rate limiting", 1, 5),
        make_edit("src/b.ts", "fix a typo", 10, 10),
        make_edit("src/c.ts", "rate limit retries", 20, 25),
    ]);
    store.write(&sha, &note).unwrap();

    let mut cache = Cache::open_in_memory().unwrap();
    cache.reindex_from(&store).unwrap();

    let hits = cache.search_prompts("rate", 10).unwrap();
    assert_eq!(hits.len(), 2);
    let prompts: Vec<&str> = hits.iter().map(|r| r.prompt.as_str()).collect();
    assert!(prompts.contains(&"implement rate limiting"));
    assert!(prompts.contains(&"rate limit retries"));
}

#[test]
fn reindex_clears_previous_cache_state() {
    let (_dir, git, sha) = fixture_repo();
    let store = NotesStore::new(git, NOTES_REF_PUBLIC);

    // First write: 2 edits.
    store
        .write(
            &sha,
            &Note::new(vec![
                make_edit("a.ts", "p1", 1, 1),
                make_edit("b.ts", "p2", 1, 1),
            ]),
        )
        .unwrap();

    let mut cache = Cache::open_in_memory().unwrap();
    cache.reindex_from(&store).unwrap();
    assert_eq!(cache.note_count().unwrap(), 1);

    // Overwrite: 1 edit.
    store
        .write(&sha, &Note::new(vec![make_edit("c.ts", "p3", 1, 1)]))
        .unwrap();
    cache.reindex_from(&store).unwrap();

    // Old prompts should be gone from FTS, new one present.
    assert!(cache.search_prompts("p1", 10).unwrap().is_empty());
    assert!(cache.search_prompts("p2", 10).unwrap().is_empty());
    let p3 = cache.search_prompts("p3", 10).unwrap();
    assert_eq!(p3.len(), 1);
}

#[test]
fn cache_get_note_returns_none_when_uncached() {
    let cache = Cache::open_in_memory().unwrap();
    assert!(cache.get_note("nonexistent").unwrap().is_none());
}

#[test]
fn store_supports_two_distinct_refs() {
    let (_dir, git, sha) = fixture_repo();
    let public = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    let private = NotesStore::new(git, "refs/notes/prompts-private");

    let pub_note = Note::new(vec![make_edit("p.ts", "public prompt", 1, 1)]);
    let priv_note = Note::new(vec![make_edit("p.ts", "private prompt", 1, 1)]);

    public.write(&sha, &pub_note).unwrap();
    private.write(&sha, &priv_note).unwrap();

    // Each ref is independent.
    assert_eq!(
        public.read(&sha).unwrap().unwrap().edits[0].prompt,
        "public prompt"
    );
    assert_eq!(
        private.read(&sha).unwrap().unwrap().edits[0].prompt,
        "private prompt"
    );
}
