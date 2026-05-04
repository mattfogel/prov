//! `prov regenerate <file>:<line>` — replay an AI-captured prompt against a
//! chosen model and diff the new output against the stored original.
//!
//! Flow:
//!  1. Resolve the line via the read-side resolver to confirm provenance.
//!  2. Read the originating note via `NotesStore` at the blame commit and
//!     pick the edit whose `line_range` covers the requested line.
//!  3. If `--root` is set, walk the `derived_from` chain to the original
//!     prompt (an AI-on-AI rewrite has a chain; backfill notes do not).
//!  4. Read the captured `original_blob_sha` content via `git cat-file blob`.
//!     If the blob is gone (gc'd), regenerate anyway and skip the diff.
//!  5. Read `ANTHROPIC_API_KEY` from env into an owned `String`. The plan
//!     also calls for `std::env::remove_var` after read to harden against
//!     subprocess env inheritance, but that's `unsafe` (forbidden by the
//!     workspace lints), so we instead `env_remove("ANTHROPIC_API_KEY")` on
//!     every subprocess we spawn here — same goal without the unsafe hatch.
//!  6. Call the Anthropic Messages API and render a unified diff against
//!     the captured original. The error type's `Display` impl strips
//!     API-key-shaped substrings before surfacing.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context};
use clap::Parser;
use similar::{ChangeTag, TextDiff};

use prov_core::resolver::{ResolveResult, Resolver};
use prov_core::schema::{DerivedFrom, Edit, Note};

use crate::anthropic::{AnthropicError, Client};

use super::common::RepoHandles;

/// Env var that lets tests point the Anthropic client at a mockito server.
/// Hidden because production never sets it.
const BASE_URL_ENV: &str = "PROV_ANTHROPIC_BASE_URL";

#[derive(Parser, Debug)]
pub struct Args {
    /// `file:line` target whose original prompt should be replayed.
    pub target: String,
    /// Override the model recorded in the note. Defaults to the captured model.
    #[arg(long)]
    pub model: Option<String>,
    /// Walk `derived_from` to the original prompt rather than the most recent.
    #[arg(long)]
    pub root: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let (file, line) = parse_target(&args.target)?;

    // Step 1 — resolve to confirm provenance and locate the originating commit.
    let handles = RepoHandles::open()?;
    let git = handles.git.clone();
    let notes = handles.notes.clone();

    let resolver = Resolver::new(handles.git.clone(), handles.notes.clone(), handles.cache);
    let _ = resolver.ensure_fresh();
    let blame_commit = match resolver.resolve(&file, line)? {
        ResolveResult::Unchanged { blame_commit, .. }
        | ResolveResult::Drifted { blame_commit, .. } => blame_commit,
        ResolveResult::NoProvenance { reason } => {
            return Err(anyhow!(
                "no provenance for {}:{line} ({reason:?}); cannot regenerate without an originating prompt",
                file.display()
            ));
        }
    };

    // Step 2 — find the originating Edit inside the note.
    let note = notes
        .read(&blame_commit)
        .context("reading originating note")?
        .ok_or_else(|| {
            anyhow!(
                "blame attributes the line to {blame_commit} but no note exists there; \
                 the cache may be stale — run `prov reindex` and retry"
            )
        })?;

    let mut edit = pick_edit_for_line(&note, &file, line)?.clone();

    // Step 3 — walk `derived_from` if --root.
    if args.root {
        edit = follow_to_root(&notes, edit)?;
    }

    // Step 4 — read the captured original blob, if reachable.
    let original_text = if let Some(sha) = edit.original_blob_sha.as_deref() {
        match read_blob(&git, sha) {
            Ok(text) => Some(text),
            Err(BlobError::Missing) => {
                eprintln!(
                    "prov: original blob {sha} no longer reachable (gc'd); \
                     regenerating without diff comparison"
                );
                None
            }
            Err(BlobError::Other(e)) => return Err(e),
        }
    } else {
        eprintln!(
            "prov: this note predates `original_blob_sha` capture; \
             regenerating without diff comparison"
        );
        None
    };

    // Step 5 — load the API key into an owned String. We deliberately do NOT
    // call `std::env::remove_var` (unsafe; forbidden by workspace lints).
    // `read_blob` already calls `env_remove` on its git subprocess so the
    // key never inherits into the only child this command spawns.
    let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
        anyhow!(
            "ANTHROPIC_API_KEY is not set in the environment. Export it before \
             running `prov regenerate`."
        )
    })?;

    // Step 6 — call Anthropic and render the diff.
    let model = args.model.as_deref().unwrap_or(&edit.model).to_string();
    let system = edit
        .preceding_turns_summary
        .as_deref()
        .filter(|s| !s.is_empty());

    let mut client = Client::new(api_key)?;
    if let Ok(base) = std::env::var(BASE_URL_ENV) {
        client = client.with_base_url(base);
    }

    let regenerated = match client.complete(&model, &edit.prompt, system) {
        Ok(text) => text,
        Err(AnthropicError::RateLimited { retry_after }) => {
            return Err(anyhow!(
                "anthropic rate-limited (HTTP 429); retry-after={}",
                retry_after.as_deref().unwrap_or("(not provided)")
            ));
        }
        Err(e) => return Err(anyhow::Error::from(e)),
    };

    print_header(&file, line, &edit, &model, args.root);
    if let Some(original) = original_text {
        print_unified_diff(&original, &regenerated);
    } else {
        println!("--- regenerated output ---");
        println!("{regenerated}");
    }
    Ok(())
}

fn parse_target(s: &str) -> anyhow::Result<(std::path::PathBuf, u32)> {
    let (path, line) = s.rsplit_once(':').ok_or_else(|| {
        anyhow!(
            "regenerate target must be `file:line` (got `{s}`); a whole-file regenerate is not supported"
        )
    })?;
    let line: u32 = line
        .parse()
        .with_context(|| format!("`{line}` is not a valid line number"))?;
    Ok((std::path::PathBuf::from(path), line))
}

/// Find the Edit whose `line_range` covers `line` for `file`. If multiple
/// edits overlap, prefer the most recent (last in array), matching the
/// resolver's tiebreaker convention.
fn pick_edit_for_line<'n>(note: &'n Note, file: &Path, line: u32) -> anyhow::Result<&'n Edit> {
    let file_str = file.to_string_lossy();
    note.edits
        .iter()
        .rev()
        .find(|e| e.file == file_str && e.line_range[0] <= line && line <= e.line_range[1])
        .ok_or_else(|| {
            anyhow!(
                "no edit in the originating note covers {}:{line}; \
                 the line may belong to a different commit's note",
                file.display()
            )
        })
}

/// Walk the `derived_from` chain to the root prompt. Stops on backfill nodes
/// (no further chain), missing sources (treats current node as root), or a
/// cycle (defensive — should not happen in well-formed notes).
fn follow_to_root(
    notes: &prov_core::storage::notes::NotesStore,
    mut edit: Edit,
) -> anyhow::Result<Edit> {
    let mut seen: std::collections::HashSet<(String, u32)> = std::collections::HashSet::new();
    while let Some(DerivedFrom::Rewrite {
        source_commit,
        source_edit,
    }) = edit.derived_from.clone()
    {
        let key = (source_commit.clone(), source_edit);
        if !seen.insert(key) {
            break;
        }
        let Some(prior_note) = notes.read(&source_commit)? else {
            break;
        };
        let Some(prior) = prior_note.edits.get(source_edit as usize).cloned() else {
            break;
        };
        edit = prior;
    }
    Ok(edit)
}

enum BlobError {
    /// Blob no longer in the object database (likely gc'd).
    Missing,
    Other(anyhow::Error),
}

/// Read a blob's contents via `git cat-file blob`.
///
/// Spawns `git` directly (not via `prov_core::git::Git`) so we can pass
/// `env_remove("ANTHROPIC_API_KEY")` and prevent the API key from
/// inheriting into the child process. This is the env-isolation half of
/// the U14 plan's "remove_var after read" guidance, achieved without the
/// unsafe-code escape hatch.
fn read_blob(git: &prov_core::git::Git, sha: &str) -> Result<String, BlobError> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git.git_dir())
        .arg("--work-tree")
        .arg(git.work_tree())
        .args(["cat-file", "blob", sha])
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .map_err(|e| BlobError::Other(anyhow!("failed to spawn git cat-file: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // git's "object not found" surfaces as one of these phrases depending
        // on git version (pre-2.40 / 2.40+ / object-format-specific). Any of
        // them means the blob is no longer reachable; everything else is a
        // genuine error worth surfacing.
        let unreachable = stderr.contains("Not a valid object name")
            || stderr.contains("does not exist")
            || stderr.contains("bad file")
            || stderr.contains("not a valid 'blob' object");
        if unreachable {
            return Err(BlobError::Missing);
        }
        return Err(BlobError::Other(anyhow!(
            "git cat-file blob {sha} failed: {stderr}"
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|e| BlobError::Other(anyhow!("blob is not valid utf-8: {e}")))
}

fn print_header(file: &Path, line: u32, edit: &Edit, model: &str, root: bool) {
    println!("prov regenerate — {}:{line}", file.display());
    println!("  prompt:    {}", edit.prompt);
    println!("  captured:  {} (turn {})", edit.timestamp, edit.turn_index);
    println!("  session:   {}", edit.conversation_id);
    println!("  captured-model: {}", edit.model);
    println!(
        "  used-model:     {model}{}",
        if root { " (--root)" } else { "" }
    );
    if let Some(summary) = &edit.preceding_turns_summary {
        if !summary.is_empty() {
            println!("  preceding-turns-summary: {summary}");
        }
    }
    println!();
}

fn print_unified_diff(original: &str, regenerated: &str) {
    let diff = TextDiff::from_lines(original, regenerated);
    println!("--- original (captured)");
    println!("+++ regenerated");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        // change.value() includes its trailing newline when present; print
        // without an extra one to keep the diff faithful to the source.
        let value = change.value();
        if value.ends_with('\n') {
            print!("{sign}{value}");
        } else {
            println!("{sign}{value}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_rejects_missing_line() {
        assert!(parse_target("src/lib.rs").is_err());
    }

    #[test]
    fn parse_target_accepts_file_and_line() {
        let (p, l) = parse_target("src/lib.rs:42").unwrap();
        assert_eq!(p, Path::new("src/lib.rs"));
        assert_eq!(l, 42);
    }

    #[test]
    fn pick_edit_picks_covering_range() {
        let note = Note::new(vec![
            Edit {
                file: "src/lib.rs".into(),
                line_range: [1, 5],
                content_hashes: vec!["a".into()],
                original_blob_sha: None,
                prompt: "first".into(),
                conversation_id: "s".into(),
                turn_index: 0,
                tool_use_id: None,
                preceding_turns_summary: None,
                model: "m".into(),
                tool: "claude-code".into(),
                timestamp: "t".into(),
                derived_from: None,
            },
            Edit {
                file: "src/lib.rs".into(),
                line_range: [10, 15],
                content_hashes: vec!["b".into()],
                original_blob_sha: None,
                prompt: "second".into(),
                conversation_id: "s".into(),
                turn_index: 1,
                tool_use_id: None,
                preceding_turns_summary: None,
                model: "m".into(),
                tool: "claude-code".into(),
                timestamp: "t".into(),
                derived_from: None,
            },
        ]);
        let edit = pick_edit_for_line(&note, Path::new("src/lib.rs"), 12).unwrap();
        assert_eq!(edit.prompt, "second");
        let edit = pick_edit_for_line(&note, Path::new("src/lib.rs"), 3).unwrap();
        assert_eq!(edit.prompt, "first");
        assert!(pick_edit_for_line(&note, Path::new("src/lib.rs"), 99).is_err());
    }
}
