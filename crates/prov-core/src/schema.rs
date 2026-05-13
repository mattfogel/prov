//! Prov v1 note schema.
//!
//! One JSON note per commit lives under `refs/notes/prompts`, containing every edit
//! attributed to that commit. Schema is versioned (`version: 1`); readers refuse to
//! parse unknown versions to surface schema drift early. Forward-compatibility within
//! v1 is preserved by tolerating unknown sibling fields (no `deny_unknown_fields`) —
//! a v1.x release adding an optional field stays parseable by older v1 readers.
//!
//! Per-line `content_hashes` enable drift detection at resolve time.
//! `original_blob_sha` is reserved for downstream consumers that want to
//! recover the AI's full original output (no v1 surface reads it today —
//! the field stays optional and forward-compatible). `derived_from` is a
//! tagged union distinguishing AI-on-AI rewrites from `prov backfill`-
//! created notes.

use serde::{Deserialize, Serialize};

/// Current note schema version. Bump when the JSON shape changes incompatibly.
///
/// Within a single major version (e.g., `1`), unknown fields on `Note`/`Edit` are
/// tolerated so v1.x readers can parse v1.y notes (y > x) by ignoring fields they
/// don't recognize.
pub const SCHEMA_VERSION: u32 = 1;

/// One note per commit.
///
/// Stored as JSON under `refs/notes/prompts` keyed by commit SHA. Contains every
/// edit that the post-commit handler matched to this commit's diff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Note {
    /// Schema version. Readers refuse unknown values.
    pub version: u32,
    /// Edits attributed to this commit.
    pub edits: Vec<Edit>,
}

impl Note {
    /// Construct a fresh note carrying the current schema version.
    #[must_use]
    pub fn new(edits: Vec<Edit>) -> Self {
        Self {
            version: SCHEMA_VERSION,
            edits,
        }
    }

    /// Serialize to canonical JSON (pretty-printed, stable ordering).
    pub fn to_json(&self) -> Result<String, SchemaError> {
        serde_json::to_string_pretty(self).map_err(|e| SchemaError::Serialize(e.to_string()))
    }

    /// Parse a note from JSON, refusing unknown schema versions.
    pub fn from_json(s: &str) -> Result<Self, SchemaError> {
        let raw: serde_json::Value =
            serde_json::from_str(s).map_err(|e| SchemaError::Deserialize(e.to_string()))?;
        let version = raw
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or(SchemaError::MissingVersion)?;
        if u32::try_from(version) != Ok(SCHEMA_VERSION) {
            return Err(SchemaError::UnknownVersion(version));
        }
        serde_json::from_value(raw).map_err(|e| SchemaError::Deserialize(e.to_string()))
    }
}

/// One edit produced by a single agent harness tool-use within one turn.
///
/// `MultiEdit` tool calls decompose into one `Edit` per inner edit so each carries
/// its own `tool_use_id` correlation handle (where available).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Edit {
    /// Repo-relative path of the file edited.
    pub file: String,
    /// Inclusive `[start, end]` line range in the post-edit content.
    pub line_range: [u32; 2],
    /// `BLAKE3` hash of each line in the edit, indexed parallel to `line_range`.
    /// Enables drift detection: a current line whose hash matches `content_hashes[i]`
    /// is unchanged since AI capture; a mismatch means a human edited the line.
    pub content_hashes: Vec<String>,
    /// Git blob SHA of the AI's full original output, when capture stored one.
    /// Reserved for downstream tooling that wants to recover the original text
    /// (no v1 CLI surface reads it today — the field stays optional and
    /// forward-compatible). Older notes did not always carry one. Absent on
    /// serialize when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_blob_sha: Option<String>,
    /// The user prompt that produced this edit (post-redaction).
    pub prompt: String,
    /// Stable agent harness session id for the conversation this edit came from.
    pub conversation_id: String,
    /// Zero-based turn index within the conversation. Pairs with `conversation_id`
    /// as the deduplication key for notes-merge resolution (U10).
    pub turn_index: u32,
    /// Per-tool-call correlation handle, when the harness surfaces it. Falls back
    /// to `None` if the platform did not provide a stable id for this edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// Short summary of the prior conversation context (post-redaction). Excludes
    /// turns marked `# prov:private` so private content never leaks via summary.
    /// Optional because v1 capture does not yet emit summaries; older notes that
    /// stored an empty string deserialize as `Some("")`, which downstream code
    /// treats identically to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preceding_turns_summary: Option<String>,
    /// Model name as captured at session start or edit time.
    pub model: String,
    /// Agent harness that produced the edit, such as "claude-code" or "codex".
    pub tool: String,
    /// ISO-8601 timestamp at edit time (turn boundary if a single turn made many edits).
    pub timestamp: String,
    /// AI-on-AI / backfill provenance link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<DerivedFrom>,
}

/// Tagged union for the `derived_from` field.
///
/// Distinguishes "this edit overwrote a prior AI edit at the same range" from
/// "this note was reconstructed by `prov backfill` from a transcript file rather
/// than captured live." Downstream rendering surfaces these differently — backfill
/// notes carry an `(approximate)` marker, rewrites surface a "previous prompt"
/// expand link.
///
/// The `Unknown` catch-all variant lets v1.x readers tolerate v1.y notes that
/// add new derivation kinds — they parse cleanly and are treated as "no
/// derivation link known to this build" rather than rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DerivedFrom {
    /// AI-on-AI rewrite: this edit replaces a previously-attributed range.
    Rewrite {
        /// Commit SHA of the prior note.
        source_commit: String,
        /// Index into the prior note's `edits[]` array.
        source_edit: u32,
    },
    /// Reconstructed by `prov backfill` rather than captured live.
    Backfill {
        /// Match confidence in `[0.0, 1.0]`.
        confidence: f32,
        /// Path of the transcript file the match came from.
        transcript_path: String,
    },
    /// Forward-compatible catch-all: a future v1.y release may add new kinds
    /// (e.g., `merge`, `revert`). Older builds parse those as `Unknown` rather
    /// than failing — the resolver / log render then treat them as "no
    /// derivation".
    #[serde(other)]
    Unknown,
}

impl DerivedFrom {
    /// Convenience: `(true, Some(confidence))` when the edit was reconstructed
    /// by `prov backfill`, `(false, None)` otherwise. Shared by every read
    /// surface (`prov log`, `prov search`, `prov pr-timeline`, the GitHub
    /// Action) so a backfilled note always renders with the `(approximate)`
    /// marker — a backfilled prompt that surfaces as a live capture violates
    /// the R14 contract.
    #[must_use]
    pub fn approximate_fields(derived: Option<&Self>) -> (bool, Option<f32>) {
        match derived {
            Some(DerivedFrom::Backfill { confidence, .. }) => (true, Some(*confidence)),
            _ => (false, None),
        }
    }
}

/// Errors raised by schema parsing.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// JSON without a `version` field.
    #[error("note JSON is missing the required `version` field")]
    MissingVersion,
    /// JSON whose `version` is not the supported schema version.
    #[error("unsupported schema version {0}; this build of prov supports v{SCHEMA_VERSION}")]
    UnknownVersion(u64),
    /// Generic serde-json deserialization error.
    #[error("note JSON deserialize failed: {0}")]
    Deserialize(String),
    /// Generic serde-json serialization error.
    #[error("note JSON serialize failed: {0}")]
    Serialize(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_edit() -> Edit {
        Edit {
            file: "src/auth.ts".into(),
            line_range: [42, 44],
            content_hashes: vec!["a1b2c3".into(), "d4e5f6".into(), "g7h8i9".into()],
            original_blob_sha: Some("deadbeefcafe".into()),
            prompt: "make this faster but readable".into(),
            conversation_id: "sess_abc123".into(),
            turn_index: 3,
            tool_use_id: Some("toolu_01abc".into()),
            preceding_turns_summary: Some(
                "Refactor of auth; previously discussed rate limiting.".into(),
            ),
            model: "claude-sonnet-4-5".into(),
            tool: "claude-code".into(),
            timestamp: "2026-04-28T11:32:00Z".into(),
            derived_from: None,
        }
    }

    #[test]
    fn roundtrip_basic_note() {
        let note = Note::new(vec![sample_edit()]);
        let json = note.to_json().expect("serialize");
        let parsed = Note::from_json(&json).expect("parse");
        assert_eq!(note, parsed);
    }

    #[test]
    fn unknown_version_is_rejected() {
        let bad = r#"{"version":99,"edits":[]}"#;
        match Note::from_json(bad) {
            Err(SchemaError::UnknownVersion(99)) => {}
            other => panic!("expected UnknownVersion(99), got {other:?}"),
        }
    }

    #[test]
    fn missing_version_is_rejected() {
        let bad = r#"{"edits":[]}"#;
        match Note::from_json(bad) {
            Err(SchemaError::MissingVersion) => {}
            other => panic!("expected MissingVersion, got {other:?}"),
        }
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // Forward-compat: a v1.x reader sees a future v1.y note with an extra field
        // and parses it cleanly rather than refusing.
        let json = r#"{
            "version": 1,
            "edits": [],
            "future_field": "some_v1.x_addition",
            "metadata": {"another": "field"}
        }"#;
        let parsed = Note::from_json(json).expect("forward compat");
        assert_eq!(parsed.edits.len(), 0);
    }

    #[test]
    fn derived_from_rewrite_roundtrips() {
        let mut edit = sample_edit();
        edit.derived_from = Some(DerivedFrom::Rewrite {
            source_commit: "abcd1234".into(),
            source_edit: 0,
        });
        let json = serde_json::to_string(&edit).unwrap();
        assert!(json.contains(r#""kind":"rewrite""#));
        let parsed: Edit = serde_json::from_str(&json).unwrap();
        assert_eq!(edit, parsed);
    }

    #[test]
    fn derived_from_unknown_kind_parses_as_unknown() {
        // A v1.y reader sees a v1.x note with a `kind` it doesn't recognize.
        // The catch-all `Unknown` variant lets it deserialize without error.
        let edit_json = r#"{
            "file": "x.rs",
            "line_range": [1, 1],
            "content_hashes": ["a"],
            "prompt": "p",
            "conversation_id": "s",
            "turn_index": 0,
            "model": "m",
            "tool": "claude-code",
            "timestamp": "2026-04-28T00:00:00Z",
            "derived_from": { "kind": "future_kind", "extra": "ignored" }
        }"#;
        let parsed: Edit = serde_json::from_str(edit_json).unwrap();
        assert_eq!(parsed.derived_from, Some(DerivedFrom::Unknown));
    }

    #[test]
    fn derived_from_backfill_roundtrips() {
        let mut edit = sample_edit();
        edit.derived_from = Some(DerivedFrom::Backfill {
            confidence: 0.78,
            transcript_path: "/Users/x/.claude/projects/-foo/sess.jsonl".into(),
        });
        let json = serde_json::to_string(&edit).unwrap();
        assert!(json.contains(r#""kind":"backfill""#));
        let parsed: Edit = serde_json::from_str(&json).unwrap();
        assert_eq!(edit, parsed);
    }

    #[test]
    fn missing_optional_tool_use_id_parses() {
        let json = r#"{
            "version": 1,
            "edits": [{
                "file": "x.rs",
                "line_range": [1, 1],
                "content_hashes": ["a"],
                "original_blob_sha": "b",
                "prompt": "p",
                "conversation_id": "s",
                "turn_index": 0,
                "preceding_turns_summary": "",
                "model": "m",
                "tool": "claude-code",
                "timestamp": "2026-04-28T00:00:00Z"
            }]
        }"#;
        let n = Note::from_json(json).unwrap();
        assert!(n.edits[0].tool_use_id.is_none());
        assert!(n.edits[0].derived_from.is_none());
    }

    #[test]
    fn missing_optional_blob_sha_and_summary_parses() {
        // Capture-side notes (U3) and `prov backfill` outputs may not stamp
        // either field; deserialization populates them as `None`.
        let json = r#"{
            "version": 1,
            "edits": [{
                "file": "x.rs",
                "line_range": [1, 1],
                "content_hashes": ["a"],
                "prompt": "p",
                "conversation_id": "s",
                "turn_index": 0,
                "model": "m",
                "tool": "claude-code",
                "timestamp": "2026-04-28T00:00:00Z"
            }]
        }"#;
        let n = Note::from_json(json).unwrap();
        assert!(n.edits[0].original_blob_sha.is_none());
        assert!(n.edits[0].preceding_turns_summary.is_none());
    }

    #[test]
    fn non_claude_tool_value_roundtrips() {
        let mut edit = sample_edit();
        edit.tool = "codex".into();
        let note = Note::new(vec![edit.clone()]);
        let json = note.to_json().expect("serialize");
        let parsed = Note::from_json(&json).expect("parse");
        assert_eq!(parsed.edits[0].tool, "codex");
        assert_eq!(parsed.edits[0], edit);
    }
}
