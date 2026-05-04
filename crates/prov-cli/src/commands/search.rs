//! `prov search <query>` — full-text search over captured prompts.
//!
//! Uses the SQLite FTS5 index populated by `Cache::reindex_from`. Exposes both
//! a human-readable rendering and a `--json` envelope for the Skill and other
//! consumers.

use std::collections::HashMap;

use clap::Parser;
use serde::Serialize;

use prov_core::schema::{DerivedFrom, Note};
use prov_core::storage::sqlite::{Cache, EditRow};

use super::common::RepoHandles;

/// Default cap on hits per query.
const DEFAULT_LIMIT: u32 = 50;

#[derive(Parser, Debug)]
pub struct Args {
    /// Search query (matched against prompt text via `SQLite` FTS5).
    pub query: String,
    /// Maximum number of hits to return.
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    pub limit: u32,
    /// Emit JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let handles = RepoHandles::open()?;
    let escaped = escape_fts(&args.query);
    // Fetch one extra so we can detect whether more results were available
    // beyond the user's requested limit without paying for a separate COUNT.
    let probe_limit = args.limit.saturating_add(1);
    let mut hits = handles.cache.search_prompts(&escaped, probe_limit)?;
    let limit_usize = args.limit as usize;
    let truncated = hits.len() > limit_usize;
    if truncated {
        hits.truncate(limit_usize);
    }

    // Pull derived_from per hit out of the cache's notes table so backfilled
    // matches surface with the (approximate) marker. Live captures pre-U15
    // never set derived_from so their hits look unchanged. Cache misses
    // degrade gracefully — an unmatched hit just reports as live, matching
    // pre-fix behavior. Per-commit lookup keeps this O(unique_commits) rather
    // than O(hits).
    let derivations = lookup_derivations(&handles.cache, &hits);

    if args.json {
        let total_matched = u32::try_from(hits.len()).unwrap_or(u32::MAX);
        let payload = SearchJson {
            query: args.query.clone(),
            hits: hits
                .iter()
                .map(|h| SearchHitJson::from_row(h, derivations.get(&derivation_key(h))))
                .collect(),
            total_matched,
            limit: args.limit,
            truncated,
            prov_version: env!("CARGO_PKG_VERSION"),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if hits.is_empty() {
        println!("no matches for {:?}", args.query);
    } else {
        println!("{} match(es) for {:?}:", hits.len(), args.query);
        for h in &hits {
            let approx = if matches!(
                derivations.get(&derivation_key(h)),
                Some(DerivedFrom::Backfill { .. })
            ) {
                "  (approximate)"
            } else {
                ""
            };
            println!(
                "  {}  {}  L{}-{}  {}{approx}",
                short_sha(&h.commit_sha),
                h.timestamp,
                h.line_start,
                h.line_end,
                h.file
            );
            println!("    {}", h.prompt);
        }
        if truncated {
            println!(
                "(showing first {} match(es); pass --limit to see more)",
                args.limit
            );
        }
    }
    Ok(())
}

/// Stable lookup key for `derivations` map: every hit corresponds to one
/// `(commit_sha, edit_idx)` pair in the source note.
fn derivation_key(row: &EditRow) -> (String, u32) {
    (row.commit_sha.clone(), row.edit_idx)
}

/// Read the source note for each unique commit in `hits` and build a map of
/// (commit_sha, edit_idx) → derived_from. Cache-miss or schema errors silently
/// skip — the hit still surfaces, just without the approximate marker.
fn lookup_derivations(cache: &Cache, hits: &[EditRow]) -> HashMap<(String, u32), DerivedFrom> {
    let mut out: HashMap<(String, u32), DerivedFrom> = HashMap::new();
    let mut seen_commits: HashMap<String, Option<Note>> = HashMap::new();
    for h in hits {
        let note = seen_commits
            .entry(h.commit_sha.clone())
            .or_insert_with(|| cache.get_note(&h.commit_sha).ok().flatten());
        let Some(note) = note.as_ref() else { continue };
        let Some(edit) = note.edits.get(h.edit_idx as usize) else {
            continue;
        };
        if let Some(d) = edit.derived_from.clone() {
            out.insert((h.commit_sha.clone(), h.edit_idx), d);
        }
    }
    out
}

/// Wrap the user-supplied query in an FTS5 phrase quote so input that contains
/// FTS operator characters (`-`, `:`, `*`, `(`, `)`, `"`) is treated as a
/// literal phrase rather than parsed as syntax. Internal `"` is doubled per
/// the FTS5 quoting convention.
fn escape_fts(q: &str) -> String {
    let inner = q.replace('"', "\"\"");
    format!("\"{inner}\"")
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

#[derive(Serialize)]
struct SearchJson {
    query: String,
    hits: Vec<SearchHitJson>,
    /// Number of hits in this response (≤ `limit`).
    total_matched: u32,
    /// Limit applied to this query.
    limit: u32,
    /// True when more matches existed beyond `limit`.
    truncated: bool,
    /// Prov version that generated the envelope (for downstream version pinning).
    prov_version: &'static str,
}

#[derive(Serialize)]
struct SearchHitJson {
    commit_sha: String,
    file: String,
    line_start: u32,
    line_end: u32,
    prompt: String,
    model: String,
    timestamp: String,
    conversation_id: String,
    /// True when the originating note was reconstructed by `prov backfill`.
    /// Consumers must surface an "approximate" disclaimer; treating a
    /// backfilled prompt as authoritative violates R14.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    approximate: bool,
    /// Backfill match confidence in `[0.0, 1.0]`, only emitted when
    /// `approximate` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    approximate_confidence: Option<f32>,
}

impl SearchHitJson {
    fn from_row(row: &EditRow, derived: Option<&DerivedFrom>) -> Self {
        let (approximate, approximate_confidence) = DerivedFrom::approximate_fields(derived);
        Self {
            commit_sha: row.commit_sha.clone(),
            file: row.file.clone(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_fts_wraps_in_phrase_quotes() {
        assert_eq!(escape_fts("dedupe window"), r#""dedupe window""#);
    }

    #[test]
    fn escape_fts_doubles_internal_quotes() {
        assert_eq!(escape_fts(r#"he said "hi""#), r#""he said ""hi""""#);
    }

    #[test]
    fn escape_fts_neutralizes_operators() {
        // FTS5's `-` is the NOT prefix; wrapping in phrase quotes neutralizes it.
        assert_eq!(escape_fts("-foo"), r#""-foo""#);
    }
}
