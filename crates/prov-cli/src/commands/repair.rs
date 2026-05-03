//! `prov repair` — walk the reflog and reattach orphaned notes after a
//! rebase/amend/squash that bypassed the post-rewrite hook.
//!
//! The post-rewrite hook (U9) is the primary way notes follow a rewritten
//! commit. But the hook is skipped when:
//! - the user installed prov in one shell and ran git from another with a
//!   different `core.hooksPath`,
//! - a wrapper tool runs git with `GIT_DIR` pointing elsewhere, or
//! - the user explicitly bypassed via `--no-verify` or env-disabled hooks.
//!
//! `prov repair` is the recovery path. It walks `git reflog` for the active
//! ref (HEAD by default), pairs old-SHA → new-SHA from rewrite events, and
//! for any new SHA that lacks a note while its old SHA still has one, copies
//! the note across.
//!
//! Both public and private notes refs are walked. Repair is idempotent — a
//! second run is a no-op once orphans have been migrated.

use std::collections::BTreeSet;

use anyhow::{anyhow, Context};
use clap::Parser;

use prov_core::git::{Git, GitError};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

use super::common::CACHE_FILENAME;

#[derive(Parser, Debug)]
pub struct Args {
    /// Days of reflog history to walk (default: 14). The reflog is local;
    /// older entries are typically pruned by `git gc`. The default keeps
    /// the lookback short to avoid surfacing long-resolved rewrites.
    #[arg(long, default_value_t = 14)]
    pub days: u32,
    /// Reflog ref to walk (default: HEAD). Most rewrite events surface in
    /// HEAD's reflog; branch reflogs catch edge cases like `git rebase`
    /// finishing on a non-checked-out branch.
    #[arg(long = "ref", default_value = "HEAD")]
    pub ref_name: String,
    /// Print what would be migrated without writing.
    #[arg(long)]
    pub dry_run: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = Git::discover(&cwd).map_err(|e| match e {
        GitError::NotARepo => anyhow!("not in a git repo"),
        other => anyhow::Error::from(other),
    })?;

    let pairs = collect_rewrite_pairs(&git, &args.ref_name, args.days)
        .with_context(|| format!("walking {} reflog", args.ref_name))?;

    if pairs.is_empty() {
        println!(
            "prov repair: no rewrite events in the last {} days on {}",
            args.days, args.ref_name
        );
        return Ok(());
    }

    let mut migrated_public = 0_u32;
    let mut migrated_private = 0_u32;
    for ref_name in [NOTES_REF_PUBLIC, NOTES_REF_PRIVATE] {
        let store = NotesStore::new(git.clone(), ref_name);
        for (old, new) in &pairs {
            if old == new {
                continue;
            }
            // Skip when the new SHA already has a note — the user (or a later
            // post-rewrite run) already migrated it. Don't clobber.
            if store.read(new).ok().flatten().is_some() {
                continue;
            }
            // Look for the orphan on the old SHA. If absent, nothing to repair.
            let Some(note) = store.read(old).ok().flatten() else {
                continue;
            };
            if args.dry_run {
                println!("prov repair (dry-run, {ref_name}): would migrate {old} → {new}");
                continue;
            }
            if let Err(e) = store.write(new, &note) {
                eprintln!("prov repair: write {new} on {ref_name} failed: {e} (orphan kept)");
                continue;
            }
            // Remove the source after a successful write. If this fails the
            // orphan stays, which is annoying but not corrupting.
            let _ = store.remove(old);
            if ref_name == NOTES_REF_PUBLIC {
                migrated_public = migrated_public.saturating_add(1);
            } else {
                migrated_private = migrated_private.saturating_add(1);
            }
        }
    }

    if args.dry_run {
        return Ok(());
    }

    if migrated_public + migrated_private > 0 {
        invalidate_cache_for(&git, &pairs);
    }

    println!(
        "prov repair: migrated {migrated_public} public + {migrated_private} private orphan note(s)"
    );
    Ok(())
}

/// Walk `git reflog show <ref>` for the last `days` days and extract
/// `(old, new)` pairs from rewrite events.
///
/// The reflog is newest-first. A rewrite event at index `i` produced
/// `entries[i].0` (new SHA); the prior entry (`entries[i+1].0`) is the SHA
/// the rewrite replaced. Entries with no predecessor (the first entry, or
/// adjacent rewrite chains where the predecessor itself is from a rewrite)
/// are still emitted — repair only acts when the old SHA actually has an
/// orphaned note.
fn collect_rewrite_pairs(
    git: &Git,
    refname: &str,
    days: u32,
) -> Result<Vec<(String, String)>, GitError> {
    let since = format!("--since={days}.days.ago");
    let raw = match git.capture(["reflog", "show", "--format=%H %gs", &since, refname]) {
        Ok(s) => s,
        // Empty/missing reflog — defensive, treat as "nothing to do".
        Err(GitError::CommandFailed { .. }) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let entries: Vec<(String, String)> = raw
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            let sha = parts.next()?.to_string();
            let subject = parts.next().unwrap_or("").to_string();
            if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            Some((sha, subject))
        })
        .collect();

    let mut out: Vec<(String, String)> = Vec::new();
    for i in 0..entries.len() {
        let (new_sha, subject) = &entries[i];
        if !is_rewrite_subject(subject) {
            continue;
        }
        let Some((old_sha, _)) = entries.get(i + 1) else {
            continue;
        };
        if old_sha == new_sha {
            continue;
        }
        out.push((old_sha.clone(), new_sha.clone()));
    }
    Ok(out)
}

fn is_rewrite_subject(subject: &str) -> bool {
    // Match git's reflog subjects for rewrite-producing commands. `rebase` and
    // `commit (amend)` are the load-bearing ones; `rebase -i` emits `rebase
    // (pick)` / `(squash)` / `(fixup)` / `(finish)` per step.
    subject.starts_with("rebase")
        || subject.starts_with("commit (amend)")
        || subject.starts_with("commit(amend)")
}

fn invalidate_cache_for(git: &Git, pairs: &[(String, String)]) {
    let cache_path = git.git_dir().join(CACHE_FILENAME);
    if !cache_path.exists() {
        return;
    }
    let Ok(mut cache) = Cache::open(&cache_path) else {
        return;
    };
    let mut touched: BTreeSet<&str> = BTreeSet::new();
    for (o, n) in pairs {
        touched.insert(o.as_str());
        touched.insert(n.as_str());
    }
    for sha in &touched {
        let _ = cache.delete_note(sha);
    }
    let public = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    let _ = cache.set_recorded_notes_ref_sha(public.ref_sha().ok().flatten().as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_subjects_classified() {
        assert!(is_rewrite_subject("rebase (start): checkout main"));
        assert!(is_rewrite_subject("rebase (pick): foo"));
        assert!(is_rewrite_subject("rebase (squash): bar"));
        assert!(is_rewrite_subject(
            "rebase (finish): returning to refs/heads/x"
        ));
        assert!(is_rewrite_subject("commit (amend): tweak"));
        assert!(!is_rewrite_subject("commit: regular"));
        assert!(!is_rewrite_subject("checkout: moving from a to b"));
    }
}
