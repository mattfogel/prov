//! `prov search <query>` — full-text search over captured prompts.
//!
//! Uses the SQLite FTS5 index populated by `Cache::reindex_from`. Exposes both
//! a human-readable rendering and a `--json` envelope for the Skill and other
//! consumers.

use clap::Parser;
use serde::Serialize;

use prov_core::storage::sqlite::EditRow;

use super::common::RepoHandles;

/// Default cap on hits per query.
const DEFAULT_LIMIT: u32 = 50;

#[derive(Parser, Debug)]
pub struct Args {
    /// Search query (matched against prompt text via `SQLite` FTS5).
    pub query: String,
    /// Emit JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let handles = RepoHandles::open()?;
    let escaped = escape_fts(&args.query);
    let hits = handles.cache.search_prompts(&escaped, DEFAULT_LIMIT)?;

    if args.json {
        let payload = SearchJson {
            query: args.query.clone(),
            hits: hits.iter().map(SearchHitJson::from).collect(),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if hits.is_empty() {
        println!("no matches for {:?}", args.query);
    } else {
        println!("{} match(es) for {:?}:", hits.len(), args.query);
        for h in &hits {
            println!(
                "  {}  {}  L{}-{}  {}",
                short_sha(&h.commit_sha),
                h.timestamp,
                h.line_start,
                h.line_end,
                h.file
            );
            println!("    {}", h.prompt);
        }
    }
    Ok(())
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
}

impl From<&EditRow> for SearchHitJson {
    fn from(row: &EditRow) -> Self {
        Self {
            commit_sha: row.commit_sha.clone(),
            file: row.file.clone(),
            line_start: row.line_start,
            line_end: row.line_end,
            prompt: row.prompt.clone(),
            model: row.model.clone(),
            timestamp: row.timestamp.clone(),
            conversation_id: row.conversation_id.clone(),
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
