//! File-based smoke tests for the transcript parser. Unit tests live alongside
//! the parser in `prov-core::transcript`; this file exercises the on-disk path
//! against synthetic fixtures so we know `parse_transcript(path)` agrees with
//! `parse_transcript_text(str)`.

use std::path::PathBuf;

use prov_core::transcript::parse_transcript;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts")
        .join(name)
}

#[test]
fn parses_basic_fixture_end_to_end() {
    let session = parse_transcript(fixture_path("basic.jsonl")).expect("parse fixture");
    assert_eq!(session.session_id, "sess-basic");
    assert_eq!(session.cwd.as_deref(), Some("/tmp/repo"));
    assert_eq!(session.model.as_deref(), Some("claude-sonnet-4-7"));

    // Two real user turns; tool_result echoes are filtered out.
    assert_eq!(session.turns.len(), 2);
    assert_eq!(session.turns[0].prompt, "add a greeting");
    assert_eq!(session.turns[1].prompt, "add tests");

    // One Edit + one Write = two edits, attributed to the right turns.
    assert_eq!(session.edits.len(), 2);
    assert_eq!(session.edits[0].file, "/tmp/repo/src/main.rs");
    assert_eq!(session.edits[0].turn_index, 0);
    assert_eq!(session.edits[0].tool_name, "Edit");
    assert_eq!(session.edits[1].file, "/tmp/repo/tests/smoke.rs");
    assert_eq!(session.edits[1].turn_index, 1);
    assert_eq!(session.edits[1].tool_name, "Write");
    assert_eq!(session.edits[1].old_string, "");
}

#[test]
fn missing_file_returns_io_error() {
    let result = parse_transcript(fixture_path("does-not-exist.jsonl"));
    assert!(matches!(
        result,
        Err(prov_core::transcript::TranscriptError::Io(_))
    ));
}
