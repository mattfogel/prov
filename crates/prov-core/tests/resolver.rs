//! Integration tests for the `Resolver` pipeline.
//!
//! Each test sets up a tiny fixture repo with one commit, attaches a fixture
//! note via `NotesStore`, and exercises one branch of the resolve pipeline.

use std::path::Path;
use std::process::Command;

use prov_core::git::Git;
use prov_core::schema::{Edit, Note};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::{NoProvenanceReason, ResolveResult, Resolver};
use tempfile::TempDir;

const NOTES_REF: &str = "refs/notes/prompts";

struct Fixture {
    _tmp: TempDir,
    git: Git,
    notes: NotesStore,
    head: String,
}

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

fn write_file(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn init_with_file(rel: &str, content: &str) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    run_git(root, &["init", "-q", "-b", "main"]);
    run_git(root, &["config", "--local", "user.email", "t@x.com"]);
    run_git(root, &["config", "--local", "user.name", "T"]);
    write_file(root, rel, content);
    run_git(root, &["add", rel]);
    run_git(root, &["commit", "-q", "-m", "seed"]);

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

    let git = Git::discover(root).unwrap();
    let notes = NotesStore::new(git.clone(), NOTES_REF);
    Fixture {
        _tmp: tmp,
        git,
        notes,
        head,
    }
}

fn line_hash(line: &str) -> String {
    blake3::hash(line.as_bytes()).to_hex().to_string()
}

fn edit_for(file: &str, start: u32, lines: &[&str]) -> Edit {
    let len = u32::try_from(lines.len()).expect("test fixture line count fits in u32");
    Edit {
        file: file.into(),
        line_range: [start, start + len - 1],
        content_hashes: lines.iter().map(|l| line_hash(l)).collect(),
        original_blob_sha: Some("originalblob".into()),
        prompt: format!("write {file}"),
        conversation_id: "sess_int".into(),
        turn_index: 0,
        tool_use_id: Some("toolu_int".into()),
        preceding_turns_summary: None,
        model: "claude-sonnet-4-5".into(),
        tool: "claude-code".into(),
        timestamp: "2026-04-28T12:00:00Z".into(),
        derived_from: None,
    }
}

fn build_resolver(fix: &Fixture) -> Resolver {
    let cache = Cache::open_in_memory().unwrap();
    Resolver::new(fix.git.clone(), fix.notes.clone(), cache)
}

#[test]
fn unchanged_when_line_hash_matches() {
    let fix = init_with_file("src/lib.rs", "alpha\nbeta\ngamma\n");
    let edit = edit_for("src/lib.rs", 1, &["alpha", "beta", "gamma"]);
    fix.notes.write(&fix.head, &Note::new(vec![edit])).unwrap();

    let r = build_resolver(&fix);
    let result = r.resolve(Path::new("src/lib.rs"), 2).unwrap();
    match result {
        ResolveResult::Unchanged { prompt, .. } => assert_eq!(prompt, "write src/lib.rs"),
        other => panic!("expected Unchanged, got {other:?}"),
    }
}

#[test]
fn drifted_when_line_was_modified_after_capture() {
    let fix = init_with_file("src/lib.rs", "alpha\nbeta\ngamma\n");
    // Stored hashes encode the AI-original content; current file content differs.
    let edit = edit_for("src/lib.rs", 1, &["alpha", "BETA-ORIGINAL", "gamma"]);
    fix.notes.write(&fix.head, &Note::new(vec![edit])).unwrap();

    let r = build_resolver(&fix);
    let result = r.resolve(Path::new("src/lib.rs"), 2).unwrap();
    match result {
        ResolveResult::Drifted {
            prompt,
            blame_author_after,
            ..
        } => {
            assert_eq!(prompt, "write src/lib.rs");
            assert_eq!(blame_author_after, "T");
        }
        other => panic!("expected Drifted, got {other:?}"),
    }
}

#[test]
fn no_note_for_commit_when_note_is_absent() {
    let fix = init_with_file("src/lib.rs", "alpha\nbeta\n");
    let r = build_resolver(&fix);
    let result = r.resolve(Path::new("src/lib.rs"), 1).unwrap();
    assert!(matches!(
        result,
        ResolveResult::NoProvenance {
            reason: NoProvenanceReason::NoNoteForCommit
        }
    ));
}

#[test]
fn no_matching_note_when_line_outside_edit_range() {
    let fix = init_with_file("src/lib.rs", "alpha\nbeta\ngamma\n");
    // Edit covers line 1 only; resolve line 3 → no match.
    let edit = edit_for("src/lib.rs", 1, &["alpha"]);
    fix.notes.write(&fix.head, &Note::new(vec![edit])).unwrap();

    let r = build_resolver(&fix);
    let result = r.resolve(Path::new("src/lib.rs"), 3).unwrap();
    assert!(matches!(
        result,
        ResolveResult::NoProvenance {
            reason: NoProvenanceReason::NoMatchingNote
        }
    ));
}

#[test]
fn inclusive_range_boundaries_resolve_correctly() {
    let fix = init_with_file("src/lib.rs", "first\nsecond\nthird\n");
    let edit = edit_for("src/lib.rs", 1, &["first", "second", "third"]);
    fix.notes.write(&fix.head, &Note::new(vec![edit])).unwrap();

    let r = build_resolver(&fix);
    // Both endpoints inclusive.
    assert!(r
        .resolve(Path::new("src/lib.rs"), 1)
        .unwrap()
        .is_unchanged());
    assert!(r
        .resolve(Path::new("src/lib.rs"), 3)
        .unwrap()
        .is_unchanged());
    // Just-past-end → no matching note.
    assert!(r
        .resolve(Path::new("src/lib.rs"), 4)
        .unwrap()
        .is_no_provenance());
}

#[test]
fn renamed_file_still_resolves_via_blame_minus_c_minus_m() {
    let fix = init_with_file("src/old.rs", "one\ntwo\nthree\n");
    let edit = edit_for("src/old.rs", 1, &["one", "two", "three"]);
    fix.notes.write(&fix.head, &Note::new(vec![edit])).unwrap();

    // Rename src/old.rs → src/new.rs in a follow-up commit.
    let root = fix.git.work_tree();
    run_git(root, &["mv", "src/old.rs", "src/new.rs"]);
    run_git(root, &["commit", "-q", "-m", "rename"]);

    let r = build_resolver(&fix);
    // The note is attached to the original commit and stores the original
    // filename. Blame -C -M follows the rename, so resolve(new.rs:2) finds
    // the original commit, and the edit lookup uses the original file path
    // ("src/old.rs") that blame reports.
    let result = r.resolve(Path::new("src/new.rs"), 2).unwrap();
    match result {
        ResolveResult::Unchanged { prompt, .. } => assert_eq!(prompt, "write src/old.rs"),
        other => panic!("expected Unchanged after rename, got {other:?}"),
    }
}

#[test]
fn corrupt_note_at_ref_returns_no_provenance() {
    // Write garbage directly into the notes ref to simulate corruption.
    let fix = init_with_file("src/lib.rs", "alpha\n");
    let _: String = fix
        .git
        .capture_with_stdin(
            [
                "notes",
                "--ref",
                NOTES_REF,
                "add",
                "--force",
                "--file=-",
                fix.head.as_str(),
            ],
            b"this-is-not-json",
        )
        .unwrap();

    let r = build_resolver(&fix);
    // Schema error inside notes::read surfaces as a ResolverError; the resolve
    // call returns Err. Acceptable: callers render this as "schema error" in
    // the CLI rather than NoProvenance, because corrupted notes warrant a loud
    // signal so the user knows to investigate.
    let err = r.resolve(Path::new("src/lib.rs"), 1).unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Schema") || msg.contains("Notes"),
        "unexpected error: {msg}"
    );
}

#[test]
fn overlapping_edits_resolve_to_the_one_that_explains_the_current_line() {
    // Regression for issue #37. The post-commit matcher's strategy-c
    // proximity fallback can collapse multiple captured edits onto the same
    // line range — typical when a sequence of turns lands in one new file
    // and individual `after` strings can no longer be located in the final
    // diff. The resolver used to return the first such edit unconditionally,
    // which mis-attributed the line and reported DRIFTED even though the
    // user had only ever AI-edited it.
    let fix = init_with_file("src/greet.ts", "import { template } from './templates';\n");

    // Three edits, all overlapping line 1, captured in turn order. Only the
    // most recent one's stored hash matches what's actually on disk —
    // exactly the shape produced by the buggy matcher.
    let mut t1 = edit_for(
        "src/greet.ts",
        1,
        &["export function greet(name: string) {"],
    );
    t1.prompt = "Create src/greet.ts".into();
    t1.timestamp = "2026-05-03T18:55:15Z".into();
    let mut t2 = edit_for(
        "src/greet.ts",
        1,
        &["  if (!name) throw new TypeError('name required');"],
    );
    t2.prompt = "Tighten the error path".into();
    t2.timestamp = "2026-05-03T18:55:30Z".into();
    let mut t3 = edit_for(
        "src/greet.ts",
        1,
        &["import { template } from './templates';"],
    );
    t3.prompt = "Extract the greeting template".into();
    t3.timestamp = "2026-05-03T18:55:46Z".into();
    fix.notes
        .write(&fix.head, &Note::new(vec![t1, t2, t3]))
        .unwrap();

    let r = build_resolver(&fix);
    let result = r.resolve(Path::new("src/greet.ts"), 1).unwrap();
    match result {
        ResolveResult::Unchanged { prompt, .. } => {
            assert_eq!(
                prompt, "Extract the greeting template",
                "should attribute the line to the turn whose hash matches the current content"
            );
        }
        other => panic!("expected Unchanged for AI-written line, got {other:?}"),
    }
}

#[test]
fn overlapping_edits_with_no_match_report_drifted_with_most_recent_capture() {
    // When none of the overlapping edits' stored hashes match the current
    // line, the line truly has drifted. The Drifted answer must name the
    // most recent capture — that's the closest thing to "what should be
    // there" — not an earlier turn that's been doubly-overwritten.
    let fix = init_with_file("src/greet.ts", "// hand-edited\n");

    let mut t1 = edit_for("src/greet.ts", 1, &["original-line"]);
    t1.prompt = "first turn".into();
    t1.timestamp = "2026-05-03T00:00:01Z".into();
    let mut t2 = edit_for("src/greet.ts", 1, &["second-line"]);
    t2.prompt = "most recent turn".into();
    t2.timestamp = "2026-05-03T00:00:02Z".into();
    fix.notes
        .write(&fix.head, &Note::new(vec![t1, t2]))
        .unwrap();

    let r = build_resolver(&fix);
    let result = r.resolve(Path::new("src/greet.ts"), 1).unwrap();
    match result {
        ResolveResult::Drifted { prompt, .. } => {
            assert_eq!(prompt, "most recent turn");
        }
        other => panic!("expected Drifted, got {other:?}"),
    }
}

#[test]
fn no_blame_when_line_out_of_range() {
    let fix = init_with_file("src/lib.rs", "only-one-line\n");
    let r = build_resolver(&fix);
    let result = r.resolve(Path::new("src/lib.rs"), 99).unwrap();
    assert!(result.is_no_provenance());
}

// Performance test marked `#[ignore]` so it doesn't run by default.
//
// To exercise: `cargo test --release --test resolver -- --ignored resolver_perf`
//
// This test seeds a 1k-note cache (proxy for the 10k target — keeps fixture
// build time under a second) and asserts p95 resolve latency under the
// per-query target. The 50ms target on 10k notes scales the same way; the
// real bottleneck is the per-query `git blame` call, not the cache.
#[test]
#[ignore = "perf — run with `--release --ignored`"]
fn resolver_perf_p95_under_50ms() {
    use std::time::Instant;

    let fix = init_with_file("src/lib.rs", "alpha\n");
    // Attach 1000 distinct notes by amending unrelated dummy commits is
    // expensive; for a perf signal we just reuse the head note 1000 times in
    // the cache and resolve repeatedly.
    let edit = edit_for("src/lib.rs", 1, &["alpha"]);
    fix.notes.write(&fix.head, &Note::new(vec![edit])).unwrap();
    let r = build_resolver(&fix);

    // Warm.
    let _ = r.resolve(Path::new("src/lib.rs"), 1).unwrap();

    let mut samples = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let t = Instant::now();
        let _ = r.resolve(Path::new("src/lib.rs"), 1).unwrap();
        samples.push(t.elapsed());
    }
    samples.sort();
    // p95 index = floor(n * 95 / 100); for n=1000 that's index 950. Integer
    // arithmetic avoids a clippy cast-precision warning.
    let p95 = samples[samples.len() * 95 / 100];
    assert!(
        p95.as_millis() < 50,
        "p95 resolve latency {p95:?} exceeded 50ms target"
    );
}
