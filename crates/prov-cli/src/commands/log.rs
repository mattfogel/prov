//! `prov log <file>[:<line>]` — surface the originating prompt for code.
//!
//! Two shapes: a point lookup (`<file>:<line>`) routes through the resolver and
//! returns drift state; a whole-file lookup (`<file>`) lists every cached edit
//! for the file, ordered by recency.
//!
//! `--history` walks the `derived_from` chain so AI-on-AI rewrites surface the
//! superseded prior prompts. `--full` is reserved for transcript expansion;
//! v1's note schema does not yet carry `transcript_path`, so the flag prints
//! the stored `preceding_turns_summary` plus a one-line note documenting the
//! limitation. `--only-if-substantial` returns empty when the file is short or
//! has no provenance — the Skill (U12) uses it to suppress noisy queries.
//!
//! `--json` switches to a machine-readable envelope so the Skill and the
//! GitHub Action can consume the same surface without re-implementing the
//! parser.

use std::path::PathBuf;

use clap::Parser;
use serde::Serialize;

use prov_core::resolver::{NoProvenanceReason, ResolveResult};
use prov_core::schema::DerivedFrom;

use super::common::RepoHandles;

/// Files shorter than this skip the lookup when `--only-if-substantial` is set.
const SUBSTANTIAL_MIN_LINES: usize = 10;

#[derive(Parser, Debug)]
pub struct Args {
    /// File path, optionally with `:<line>` suffix.
    pub target: String,
    /// Show provenance history including superseded prompts.
    #[arg(long)]
    pub history: bool,
    /// Expand `preceding_turns_summary` into the full transcript.
    #[arg(long)]
    pub full: bool,
    /// Skip the lookup if the file has fewer than N lines or no existing notes.
    #[arg(long)]
    pub only_if_substantial: bool,
    /// Emit JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let (file, line) = parse_target(&args.target);

    let handles = RepoHandles::open()?;

    if args.only_if_substantial && !is_substantial(&handles, &file)? {
        emit_empty(args.json, &file);
        return Ok(());
    }

    match line {
        Some(n) => point_lookup(handles, &file, n, &args),
        None => whole_file_lookup(handles, &file, &args),
    }
}

fn parse_target(s: &str) -> (PathBuf, Option<u32>) {
    if let Some((path, line)) = s.rsplit_once(':') {
        if let Ok(n) = line.parse::<u32>() {
            return (PathBuf::from(path), Some(n));
        }
    }
    (PathBuf::from(s), None)
}

fn is_substantial(handles: &RepoHandles, file: &std::path::Path) -> anyhow::Result<bool> {
    // Short-file gate. Newline count is a fine proxy for line count here —
    // bytecount adds a dependency without measurable benefit at this scale.
    let abs = handles.git.work_tree().join(file);
    if let Ok(bytes) = std::fs::read(&abs) {
        #[allow(clippy::naive_bytecount)]
        let lines = bytes.iter().filter(|&&b| b == b'\n').count() + 1;
        if lines < SUBSTANTIAL_MIN_LINES {
            return Ok(false);
        }
    }
    // No-notes gate. Cache miss is the common case immediately after install;
    // treating a fresh cache as "not substantial" keeps the Skill quiet until
    // the user has actually committed AI-generated code.
    let edits = handles.cache.edits_for_file(&file.to_string_lossy())?;
    Ok(!edits.is_empty())
}

fn point_lookup(
    handles: RepoHandles,
    file: &std::path::Path,
    line: u32,
    args: &Args,
) -> anyhow::Result<()> {
    let resolver = handles.into_resolver();
    let _ = resolver.ensure_fresh();
    let result = resolver.resolve(file, line)?;

    if args.json {
        let payload = PointJson::from(&result, file, line);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        render_point_text(&result, file, line, args.full);
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn whole_file_lookup(
    handles: RepoHandles,
    file: &std::path::Path,
    args: &Args,
) -> anyhow::Result<()> {
    let file_str = file.to_string_lossy().to_string();
    let edits = handles.cache.edits_for_file(&file_str)?;
    let derivations = lookup_derivations(&handles, &edits);

    let history = if args.history {
        load_history_chain(&handles, &edits)?
    } else {
        Vec::new()
    };

    if args.json {
        let payload = WholeFileJson {
            file: file_str.clone(),
            edits: edits
                .iter()
                .zip(derivations.iter())
                .map(|(row, derived)| EditJson::from_row(row, derived.as_ref()))
                .collect(),
            history: history.iter().map(EditJson::from_history).collect(),
            prov_version: env!("CARGO_PKG_VERSION"),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        render_whole_file_text(&file_str, &edits, &derivations, &history, args.full);
    }
    Ok(())
}

/// Look up the `derived_from` field of each cached edit. Returns the same
/// length and order as `edits` so callers can zip the two together when
/// rendering. A cache miss or a missing edit index yields `None`, which
/// downstream renders treat the same as live capture.
fn lookup_derivations(
    handles: &RepoHandles,
    edits: &[prov_core::storage::sqlite::EditRow],
) -> Vec<Option<DerivedFrom>> {
    let mut out = Vec::with_capacity(edits.len());
    let mut note_cache: std::collections::HashMap<String, Option<prov_core::schema::Note>> =
        std::collections::HashMap::new();
    for row in edits {
        let entry = note_cache
            .entry(row.commit_sha.clone())
            .or_insert_with(|| handles.cache.get_note(&row.commit_sha).ok().flatten());
        let derived = entry
            .as_ref()
            .and_then(|n| n.edits.get(row.edit_idx as usize))
            .and_then(|e| e.derived_from.clone());
        out.push(derived);
    }
    out
}

/// Walk every edit's `derived_from` chain into a flat list of superseded prior edits.
fn load_history_chain(
    handles: &RepoHandles,
    edits: &[prov_core::storage::sqlite::EditRow],
) -> anyhow::Result<Vec<HistoryEntry>> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<(String, u32)> = std::collections::HashSet::new();

    for edit in edits {
        let Some(note) = handles.cache.get_note(&edit.commit_sha)? else {
            continue;
        };
        let Some(this_edit) = note.edits.get(edit.edit_idx as usize) else {
            continue;
        };

        let mut current_derived = this_edit.derived_from.clone();
        while let Some(DerivedFrom::Rewrite {
            ref source_commit,
            source_edit,
        }) = current_derived
        {
            let key = (source_commit.clone(), source_edit);
            if !seen.insert(key.clone()) {
                break;
            }
            let next_derived = match handles.cache.get_note(source_commit)? {
                Some(prior_note) => match prior_note.edits.get(source_edit as usize) {
                    Some(prior) => {
                        let derived_next = prior.derived_from.clone();
                        out.push(HistoryEntry {
                            commit_sha: source_commit.clone(),
                            edit: prior.clone(),
                        });
                        derived_next
                    }
                    None => break,
                },
                None => match handles.notes.read(source_commit)? {
                    Some(n) => match n.edits.get(source_edit as usize) {
                        Some(prior) => {
                            let derived_next = prior.derived_from.clone();
                            out.push(HistoryEntry {
                                commit_sha: source_commit.clone(),
                                edit: prior.clone(),
                            });
                            derived_next
                        }
                        None => break,
                    },
                    None => break,
                },
            };
            current_derived = next_derived;
        }
    }
    Ok(out)
}

struct HistoryEntry {
    commit_sha: String,
    edit: prov_core::schema::Edit,
}

fn emit_empty(json: bool, file: &std::path::Path) {
    if json {
        // Match `WholeFileJson`'s shape so downstream consumers can deserialize
        // both with one struct.
        let payload = WholeFileJson {
            file: file.to_string_lossy().into_owned(),
            edits: Vec::new(),
            history: Vec::new(),
            prov_version: env!("CARGO_PKG_VERSION"),
        };
        if let Ok(s) = serde_json::to_string(&payload) {
            println!("{s}");
        }
    }
    // Non-JSON: stay silent. The Skill consumes JSON; humans calling
    // `--only-if-substantial` already know to interpret silence as "skip".
}

// ----- text rendering -----

fn render_point_text(result: &ResolveResult, file: &std::path::Path, line: u32, full: bool) {
    match result {
        ResolveResult::Unchanged {
            prompt,
            model,
            timestamp,
            conversation_id,
            turn_index,
            blame_commit,
            derived_from,
            ..
        } => {
            println!("{}:{line}", file.display());
            println!("  status:    unchanged since AI capture");
            println!("  prompt:    {prompt}");
            println!("  model:     {model}");
            println!("  captured:  {timestamp}");
            println!("  session:   {conversation_id} (turn {turn_index})");
            println!("  commit:    {blame_commit}");
            if let Some(approx) = approximate_label(derived_from.as_ref()) {
                println!("  source:    {approx}");
            }
            if full {
                println!();
                println!("  --full: transcript expansion is not yet shipped (v1.x); the");
                println!("  preceding-turns summary is stored on the note and can be inspected");
                println!("  via `prov log {} --json`.", file.display());
            }
        }
        ResolveResult::Drifted {
            prompt,
            model,
            timestamp,
            conversation_id,
            turn_index,
            blame_author_after,
            blame_commit,
            derived_from,
            ..
        } => {
            println!("{}:{line}", file.display());
            println!("  status:    DRIFTED — current line content does not match AI capture");
            println!("  prompt:    {prompt}");
            println!("  model:     {model}");
            println!("  captured:  {timestamp}");
            println!("  session:   {conversation_id} (turn {turn_index})");
            println!("  commit:    {blame_commit}");
            println!("  drifted_by: {blame_author_after}");
            if let Some(approx) = approximate_label(derived_from.as_ref()) {
                println!("  source:    {approx}");
            }
            if full {
                println!();
                println!("  --full: transcript expansion is not yet shipped (v1.x).");
            }
        }
        ResolveResult::NoProvenance { reason } => {
            println!(
                "{}:{line}: no provenance ({})",
                file.display(),
                describe_reason(reason)
            );
        }
    }
}

/// Render a `Backfill` derivation as a human-readable label. Returns `None`
/// for live-captured or `Rewrite`-derived edits — those are authoritative and
/// don't need an annotation.
fn approximate_label(derived: Option<&DerivedFrom>) -> Option<String> {
    match derived? {
        DerivedFrom::Backfill { confidence, .. } => Some(format!(
            "(approximate) reconstructed by `prov backfill` (confidence {confidence:.2})"
        )),
        _ => None,
    }
}

fn describe_reason(r: &NoProvenanceReason) -> &'static str {
    match r {
        NoProvenanceReason::NoBlame => "git blame produced no attribution",
        NoProvenanceReason::NoNoteForCommit => "no note attached to the originating commit",
        NoProvenanceReason::NoMatchingNote => "no edit covers this line in the note",
        NoProvenanceReason::SchemaError(_) => "note JSON failed schema validation",
    }
}

fn render_whole_file_text(
    file: &str,
    edits: &[prov_core::storage::sqlite::EditRow],
    derivations: &[Option<DerivedFrom>],
    history: &[HistoryEntry],
    full: bool,
) {
    if edits.is_empty() {
        println!("{file}: no provenance");
        return;
    }
    println!("{file} — {} captured edit(s)", edits.len());
    for (e, derived) in edits.iter().zip(derivations.iter()) {
        let approx = if matches!(derived, Some(DerivedFrom::Backfill { .. })) {
            "  (approximate)"
        } else {
            ""
        };
        println!(
            "  L{}-{}  {}  {}  {}{approx}",
            e.line_start,
            e.line_end,
            e.timestamp,
            &e.commit_sha[..short_sha_len(&e.commit_sha)],
            e.model
        );
        println!("    {}", e.prompt);
    }
    if !history.is_empty() {
        println!();
        println!("history (superseded prior prompts):");
        for h in history {
            println!(
                "  {}  L{}-{}  {}",
                &h.commit_sha[..short_sha_len(&h.commit_sha)],
                h.edit.line_range[0],
                h.edit.line_range[1],
                h.edit.timestamp
            );
            println!("    {}", h.edit.prompt);
        }
    }
    if full {
        println!();
        println!("--full: transcript expansion is not yet shipped (v1.x).");
    }
}

fn short_sha_len(sha: &str) -> usize {
    sha.len().min(8)
}

// ----- JSON envelopes -----

#[derive(Serialize)]
struct PointJson {
    file: String,
    line: u32,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blame_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blame_author_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_provenance_reason: Option<String>,
    /// True when the prompt was reconstructed by `prov backfill` rather than
    /// captured live; consumers (Skill, Action) should surface an "approximate"
    /// disclaimer when set.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    approximate: bool,
    /// Backfill match confidence in `[0.0, 1.0]`. Only emitted when
    /// `approximate` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    approximate_confidence: Option<f32>,
    /// Prov version that emitted this envelope (for downstream version pinning).
    prov_version: &'static str,
}

impl PointJson {
    #[allow(clippy::too_many_lines)]
    fn from(result: &ResolveResult, file: &std::path::Path, line: u32) -> Self {
        let file = file.to_string_lossy().to_string();
        match result {
            ResolveResult::Unchanged {
                prompt,
                model,
                timestamp,
                conversation_id,
                turn_index,
                blame_commit,
                derived_from,
                ..
            } => {
                let (approximate, approximate_confidence) =
                    DerivedFrom::approximate_fields(derived_from.as_ref());
                Self {
                    file,
                    line,
                    status: "unchanged",
                    prompt: Some(prompt.clone()),
                    model: Some(model.clone()),
                    timestamp: Some(timestamp.clone()),
                    conversation_id: Some(conversation_id.clone()),
                    turn_index: Some(*turn_index),
                    blame_commit: Some(blame_commit.clone()),
                    blame_author_after: None,
                    no_provenance_reason: None,
                    approximate,
                    approximate_confidence,
                    prov_version: env!("CARGO_PKG_VERSION"),
                }
            }
            ResolveResult::Drifted {
                prompt,
                model,
                timestamp,
                conversation_id,
                turn_index,
                blame_author_after,
                blame_commit,
                derived_from,
                ..
            } => {
                let (approximate, approximate_confidence) =
                    DerivedFrom::approximate_fields(derived_from.as_ref());
                Self {
                    file,
                    line,
                    status: "drifted",
                    prompt: Some(prompt.clone()),
                    model: Some(model.clone()),
                    timestamp: Some(timestamp.clone()),
                    conversation_id: Some(conversation_id.clone()),
                    turn_index: Some(*turn_index),
                    blame_commit: Some(blame_commit.clone()),
                    blame_author_after: Some(blame_author_after.clone()),
                    no_provenance_reason: None,
                    approximate,
                    approximate_confidence,
                    prov_version: env!("CARGO_PKG_VERSION"),
                }
            }
            ResolveResult::NoProvenance { reason } => Self {
                file,
                line,
                status: "no_provenance",
                prompt: None,
                model: None,
                timestamp: None,
                conversation_id: None,
                turn_index: None,
                blame_commit: None,
                blame_author_after: None,
                no_provenance_reason: Some(describe_reason(reason).into()),
                approximate: false,
                approximate_confidence: None,
                prov_version: env!("CARGO_PKG_VERSION"),
            },
        }
    }
}

#[derive(Serialize)]
struct WholeFileJson {
    file: String,
    edits: Vec<EditJson>,
    history: Vec<EditJson>,
    /// Prov version that emitted this envelope (for downstream version pinning).
    prov_version: &'static str,
}

#[derive(Serialize)]
struct EditJson {
    commit_sha: String,
    line_start: u32,
    line_end: u32,
    prompt: String,
    model: String,
    timestamp: String,
    conversation_id: String,
    /// True when this edit was reconstructed by `prov backfill`. Consumers
    /// should annotate such entries as approximate.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    approximate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    approximate_confidence: Option<f32>,
}

impl EditJson {
    fn from_row(row: &prov_core::storage::sqlite::EditRow, derived: Option<&DerivedFrom>) -> Self {
        let (approximate, approximate_confidence) = DerivedFrom::approximate_fields(derived);
        Self {
            commit_sha: row.commit_sha.clone(),
            line_start: row.line_start,
            line_end: row.line_end,
            prompt: row.prompt.clone(),
            model: row.model.clone(),
            timestamp: row.timestamp.clone(),
            conversation_id: row.conversation_id.clone(),
            approximate,
            approximate_confidence,
        }
    }

    fn from_history(h: &HistoryEntry) -> Self {
        let (approximate, approximate_confidence) =
            DerivedFrom::approximate_fields(h.edit.derived_from.as_ref());
        Self {
            commit_sha: h.commit_sha.clone(),
            line_start: h.edit.line_range[0],
            line_end: h.edit.line_range[1],
            prompt: h.edit.prompt.clone(),
            model: h.edit.model.clone(),
            timestamp: h.edit.timestamp.clone(),
            conversation_id: h.edit.conversation_id.clone(),
            approximate,
            approximate_confidence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parse_target_handles_file_only() {
        let (p, l) = parse_target("src/lib.rs");
        assert_eq!(p, Path::new("src/lib.rs"));
        assert!(l.is_none());
    }

    #[test]
    fn parse_target_handles_file_and_line() {
        let (p, l) = parse_target("src/lib.rs:42");
        assert_eq!(p, Path::new("src/lib.rs"));
        assert_eq!(l, Some(42));
    }

    #[test]
    fn parse_target_strips_only_trailing_numeric() {
        // A path with a colon but non-numeric tail stays as-is.
        let (p, l) = parse_target("path/with:colon");
        assert_eq!(p, Path::new("path/with:colon"));
        assert!(l.is_none());
    }
}
