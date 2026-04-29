//! `prov pr-timeline --base <ref> --head <ref>` — render the PR intent timeline.
//!
//! Walks the diff between `base` and `head`, runs the resolver against every
//! added line in head, aggregates by `(conversation_id, turn_index)`, and
//! emits Markdown (the GitHub Action's comment body) or JSON (for downstream
//! tooling). The Action invokes this command directly so the comment shape has
//! a single source of truth — no parallel TypeScript renderer.
//!
//! Superseded turns: a turn whose edits were entirely overwritten by later
//! turns surface as ~~strikethrough~~. Detection compares the union of turns
//! across every commit in `base..head` against the turns reachable via blame
//! on head; turns in the former but not the latter are superseded.

use std::collections::BTreeMap;

use anyhow::Context;
use clap::Parser;

use prov_core::resolver::{ResolveResult, Resolver};
use prov_core::storage::notes::NotesStore;

use super::common::RepoHandles;
use crate::render::timeline::{SupersededTurnInfo, TimelineBuilder, TurnLineInfo};

/// Cap on resolver lookups per timeline build. Beyond this, remaining lines
/// fall into the "lines without provenance" footer rather than triggering
/// per-line blame on a monorepo-scale PR.
const MAX_LINES_RESOLVED: u32 = 5_000;

#[derive(Parser, Debug)]
pub struct Args {
    /// Base ref of the diff (e.g., the PR's target branch).
    #[arg(long)]
    pub base: String,
    /// Head ref of the diff (e.g., HEAD).
    #[arg(long)]
    pub head: String,
    /// Emit JSON (default for the GitHub Action's structured payload).
    #[arg(long, conflicts_with = "markdown")]
    pub json: bool,
    /// Emit Markdown ready to post as a PR comment.
    #[arg(long, conflicts_with = "json")]
    pub markdown: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let handles = RepoHandles::open()?;
    let notes_for_super = handles.notes.clone();
    let cache_for_super = open_supplementary_cache(&handles)?;

    let added_lines = added_lines_in_range(&handles.git, &args.base, &args.head)
        .context("failed to compute diff between base and head")?;
    let pr_commits = commits_between(&handles.git, &args.base, &args.head)
        .context("failed to enumerate commits between base and head")?;

    let resolver: Resolver = handles.into_resolver();
    let _ = resolver.ensure_fresh();

    let mut builder = TimelineBuilder::new(env!("CARGO_PKG_VERSION"));
    let mut surviving_turns: std::collections::HashSet<(String, u32)> =
        std::collections::HashSet::new();

    let mut lookups_done = 0_u32;
    'outer: for (file, lines) in &added_lines {
        for &line in lines {
            if lookups_done >= MAX_LINES_RESOLVED {
                builder.add_no_provenance_line(file, line);
                continue;
            }
            lookups_done = lookups_done.saturating_add(1);
            match resolver.resolve(std::path::Path::new(file), line)? {
                ResolveResult::Unchanged {
                    prompt,
                    model,
                    timestamp,
                    conversation_id,
                    turn_index,
                    ..
                }
                | ResolveResult::Drifted {
                    prompt,
                    model,
                    timestamp,
                    conversation_id,
                    turn_index,
                    ..
                } => {
                    surviving_turns.insert((conversation_id.clone(), turn_index));
                    builder.add_turn_line(&TurnLineInfo {
                        file,
                        conversation_id: &conversation_id,
                        turn_index,
                        prompt: &prompt,
                        model: &model,
                        timestamp: &timestamp,
                    });
                }
                ResolveResult::NoProvenance { .. } => {
                    builder.add_no_provenance_line(file, line);
                }
            }
            if lookups_done >= MAX_LINES_RESOLVED {
                // Mark the rest of this file's lines as no-provenance and bail.
                for &later in lines.iter().filter(|&&l| l > line) {
                    builder.add_no_provenance_line(file, later);
                }
                break 'outer;
            }
        }
    }

    add_superseded(
        &mut builder,
        &cache_for_super,
        &notes_for_super,
        &pr_commits,
        &surviving_turns,
    )?;

    let timeline = builder.build();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&timeline)?);
    } else {
        // Default to Markdown when neither flag is given — local CLI users
        // expect human-readable output; the Action passes `--markdown`
        // explicitly per the plan.
        print!("{}", timeline.to_markdown());
    }
    Ok(())
}

fn open_supplementary_cache(
    handles: &RepoHandles,
) -> anyhow::Result<prov_core::storage::sqlite::Cache> {
    // We need a separate Cache handle for the superseded-turn pass because
    // `into_resolver()` consumes the original. Opening twice on the same file
    // is safe — SQLite handles concurrent readers in WAL mode and each handle
    // applies its own schema-init no-op.
    Ok(prov_core::storage::sqlite::Cache::open(
        &handles.cache_path,
    )?)
}

fn add_superseded(
    builder: &mut TimelineBuilder,
    cache: &prov_core::storage::sqlite::Cache,
    notes: &NotesStore,
    pr_commits: &[String],
    surviving: &std::collections::HashSet<(String, u32)>,
) -> anyhow::Result<()> {
    // For every commit between base..head, walk its note's edits and surface
    // any (conversation_id, turn_index) that did NOT survive into head's blame.
    let mut already_added: std::collections::HashSet<(String, u32)> = surviving.clone();

    for sha in pr_commits {
        let note = match cache.get_note(sha)? {
            Some(n) => n,
            None => match notes.read(sha)? {
                Some(n) => n,
                None => continue,
            },
        };
        for edit in &note.edits {
            let key = (edit.conversation_id.clone(), edit.turn_index);
            if already_added.contains(&key) {
                continue;
            }
            already_added.insert(key);
            builder.add_superseded_turn(&SupersededTurnInfo {
                conversation_id: &edit.conversation_id,
                turn_index: edit.turn_index,
                prompt: &edit.prompt,
                model: &edit.model,
                timestamp: &edit.timestamp,
            });
        }
    }
    Ok(())
}

/// Collect added line numbers per file in `head` between `base..head`.
///
/// Uses `git diff --unified=0 --no-color base..head`; parses the `@@ -X,Y +A,B @@`
/// hunk headers to find the head-side range and synthesizes the line list.
/// Returns a `BTreeMap` so files are iterated in stable order.
fn added_lines_in_range(
    git: &prov_core::git::Git,
    base: &str,
    head: &str,
) -> anyhow::Result<BTreeMap<String, Vec<u32>>> {
    let range = format!("{base}..{head}");
    let raw = git.capture(["diff", "--unified=0", "--no-color", "--no-renames", &range])?;
    Ok(parse_diff_added_lines(&raw))
}

/// Parse `git diff --unified=0` output into per-file added line numbers.
pub(crate) fn parse_diff_added_lines(raw: &str) -> BTreeMap<String, Vec<u32>> {
    let mut out: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    let mut current_file: Option<String> = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            // `+++ b/<path>` for a file that exists on the head side; `/dev/null`
            // for deletions (no head content to attribute).
            current_file = if rest == "/dev/null" {
                None
            } else if let Some(path) = rest.strip_prefix("b/") {
                Some(path.to_string())
            } else {
                Some(rest.to_string())
            };
        } else if line.starts_with("@@ ") {
            if let (Some(file), Some((start, count))) = (&current_file, parse_hunk_plus(line)) {
                if count == 0 {
                    continue;
                }
                let entry = out.entry(file.clone()).or_default();
                for i in 0..count {
                    entry.push(start.saturating_add(i));
                }
            }
        }
    }
    out
}

/// Extract the `(start, count)` of the head-side range from a `@@ -X,Y +A,B @@` header.
fn parse_hunk_plus(line: &str) -> Option<(u32, u32)> {
    let after_at = line.strip_prefix("@@ ")?;
    let mut tokens = after_at.split_whitespace();
    let _minus = tokens.next()?; // `-X[,Y]`
    let plus = tokens.next()?; // `+A[,B]`
    let plus = plus.strip_prefix('+')?;
    if let Some((a, b)) = plus.split_once(',') {
        Some((a.parse().ok()?, b.parse().ok()?))
    } else {
        Some((plus.parse().ok()?, 1))
    }
}

/// Enumerate every commit reachable from `head` but not `base`, oldest first.
fn commits_between(
    git: &prov_core::git::Git,
    base: &str,
    head: &str,
) -> anyhow::Result<Vec<String>> {
    let range = format!("{base}..{head}");
    let raw = git.capture(["rev-list", "--reverse", &range])?;
    Ok(raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hunk_plus_handles_default_count() {
        assert_eq!(parse_hunk_plus("@@ -10 +11 @@"), Some((11, 1)));
    }

    #[test]
    fn parse_hunk_plus_handles_explicit_count() {
        assert_eq!(parse_hunk_plus("@@ -10,3 +11,5 @@"), Some((11, 5)));
    }

    #[test]
    fn parse_hunk_plus_handles_pure_deletion() {
        assert_eq!(parse_hunk_plus("@@ -10,3 +9,0 @@"), Some((9, 0)));
    }

    #[test]
    fn parse_diff_collects_added_lines_per_file() {
        let raw = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,0 +2,3 @@
+x
+y
+z
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -5,1 +5,2 @@
+w
+v
";
        let map = parse_diff_added_lines(raw);
        assert_eq!(map.get("src/a.rs"), Some(&vec![2, 3, 4]));
        assert_eq!(map.get("src/b.rs"), Some(&vec![5, 6]));
    }

    #[test]
    fn parse_diff_skips_deletions() {
        let raw = "\
diff --git a/x.rs b/x.rs
deleted file mode 100644
--- a/x.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-a
-b
-c
";
        let map = parse_diff_added_lines(raw);
        assert!(map.is_empty());
    }
}
