//! `prov gc` — periodic housekeeping for the notes ref and the staging tree.
//!
//! Three jobs:
//!
//! 1. **Cull notes for unreachable commits.** A note attached to a commit
//!    that no ref reaches anymore (force-pushed away, branch deleted) is
//!    dead weight. The note blob remains in the repo's object database
//!    until git's own gc, but pulling the entry off the notes ref is cheap
//!    and shrinks `git notes list` output.
//!
//! 2. **Prune stale staging entries.** A session whose `prov hook stop`
//!    never fired (Claude Code crashed, the user killed the terminal) leaves
//!    a `.git/prov-staging/<session_id>/` dir that the post-commit handler
//!    will never match. Default TTL is 14 days — long enough that a
//!    weekend-paused session survives, short enough that the staging tree
//!    doesn't grow without bound.
//!
//! 3. **Optional `--compact`.** Notes older than 90 days have their
//!    `preceding_turns_summary` and unreachable `original_blob_sha` dropped
//!    so the note JSON shrinks. Body text is preserved — compact is a
//!    storage optimization, not a content scrub.
//!
//! Cache is invalidated for any commit whose note changed so the next
//! resolver call repopulates from the (now-smaller) notes ref.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use clap::Parser;

use prov_core::git::{Git, GitError};
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::staging::{Staging, STAGING_DIRNAME};
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

use super::common::CACHE_FILENAME;

const DEFAULT_STAGING_TTL_DAYS: u32 = 14;
const COMPACT_AGE_DAYS: u32 = 90;

#[derive(Parser, Debug)]
pub struct Args {
    /// Also rewrite notes older than the compaction threshold (90d) to drop bulky fields.
    #[arg(long)]
    pub compact: bool,
    /// Override the staging-prune TTL (default: 14 days).
    #[arg(long, default_value_t = DEFAULT_STAGING_TTL_DAYS)]
    pub staging_ttl_days: u32,
    /// Print what would change without writing.
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

    let mut culled_public = 0_u32;
    let mut culled_private = 0_u32;
    for ref_name in [NOTES_REF_PUBLIC, NOTES_REF_PRIVATE] {
        let n = cull_unreachable(&git, ref_name, args.dry_run)
            .with_context(|| format!("culling unreachable notes on {ref_name}"))?;
        if ref_name == NOTES_REF_PUBLIC {
            culled_public = n;
        } else {
            culled_private = n;
        }
    }

    let pruned_sessions = prune_staging(&git, args.staging_ttl_days, args.dry_run);

    let mut compacted = 0_u32;
    if args.compact {
        for ref_name in [NOTES_REF_PUBLIC, NOTES_REF_PRIVATE] {
            compacted = compacted.saturating_add(
                compact_old_notes(&git, ref_name, args.dry_run)
                    .with_context(|| format!("compacting old notes on {ref_name}"))?,
            );
        }
    }

    if !args.dry_run && (culled_public + culled_private + compacted) > 0 {
        invalidate_cache(&git);
    }

    let label = if args.dry_run { " (dry-run)" } else { "" };
    println!(
        "prov gc{label}: culled {culled_public} public + {culled_private} private unreachable note(s); \
         pruned {pruned_sessions} stale staging session(s); \
         compacted {compacted} note(s)"
    );
    Ok(())
}

/// Return true if `commit_sha` is reachable from any ref. Uses
/// `git rev-list --no-walk --all <sha>`-style logic; the practical check is
/// `git cat-file -e <sha>` (object exists) AND `git merge-base --is-ancestor
/// <sha> <ref>` for at least one ref. Cheaper: ask git for every ref tip and
/// see if `git merge-base --is-ancestor` succeeds for any.
///
/// The simplest correct path: `git for-each-ref --contains <sha>` — git
/// returns the list of refs that reach the commit. Empty output ⇒ unreachable.
fn is_reachable(git: &Git, commit_sha: &str) -> bool {
    // Defensive: missing object is by definition unreachable. Guard with an
    // explicit existence check so for-each-ref doesn't error noisily.
    if git.run(["cat-file", "-e", commit_sha]).is_err() {
        return false;
    }
    match git.capture(["for-each-ref", "--contains", commit_sha]) {
        Ok(s) => !s.trim().is_empty(),
        Err(_) => false,
    }
}

fn cull_unreachable(git: &Git, ref_name: &str, dry_run: bool) -> Result<u32, GitError> {
    let store = NotesStore::new(git.clone(), ref_name);
    let entries = match store.list() {
        Ok(v) => v,
        Err(prov_core::storage::notes::NotesError::Git(e)) => return Err(e),
        Err(prov_core::storage::notes::NotesError::Schema(_)) => {
            // A note we can't parse isn't reachable for our purposes; leave it
            // alone — the user can re-run after upgrading prov.
            return Ok(0);
        }
    };

    let mut culled = 0_u32;
    for (sha, _note) in entries {
        if is_reachable(git, &sha) {
            continue;
        }
        if dry_run {
            println!("prov gc (dry-run, {ref_name}): would cull note for unreachable {sha}");
            continue;
        }
        if let Err(e) = store.remove(&sha) {
            eprintln!("prov gc: removing note for {sha} on {ref_name} failed: {e}");
            continue;
        }
        culled = culled.saturating_add(1);
    }
    Ok(culled)
}

/// Walk `<git-dir>/prov-staging/<session>/` and remove session dirs whose most
/// recent file mtime is older than `ttl_days`. Defensive: a session that's
/// currently being written to (modified within TTL) is preserved.
fn prune_staging(git: &Git, ttl_days: u32, dry_run: bool) -> u32 {
    let staging_root = git.git_dir().join(STAGING_DIRNAME);
    if !staging_root.exists() {
        return 0;
    }
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(
        u64::from(ttl_days).saturating_mul(86_400),
    )) else {
        return 0;
    };
    let staging = Staging::new(git.git_dir());
    let sessions = staging.list_sessions().unwrap_or_default();
    let mut pruned = 0_u32;
    for sid in sessions {
        let dir = staging.session_dir(&sid, false);
        let last_mtime = newest_mtime(&dir).unwrap_or_else(SystemTime::now);
        if last_mtime >= cutoff {
            continue;
        }
        if dry_run {
            println!(
                "prov gc (dry-run): would prune staging session {} (idle ≥ {ttl_days}d)",
                sid.as_str()
            );
            continue;
        }
        if let Err(e) = staging.remove_session(&sid) {
            eprintln!(
                "prov gc: removing staging session {} failed: {e}",
                sid.as_str()
            );
            continue;
        }
        pruned = pruned.saturating_add(1);
    }
    pruned
}

/// Compact notes older than [`COMPACT_AGE_DAYS`] by clearing
/// `preceding_turns_summary` and unreachable `original_blob_sha` fields.
/// "Older" is measured by the note's most recent edit `timestamp` (ISO-8601
/// strings sort lexicographically when zero-padded). Notes whose edits all
/// have empty/unparseable timestamps are skipped.
fn compact_old_notes(git: &Git, ref_name: &str, dry_run: bool) -> Result<u32, GitError> {
    let store = NotesStore::new(git.clone(), ref_name);
    let entries = match store.list() {
        Ok(v) => v,
        Err(prov_core::storage::notes::NotesError::Git(e)) => return Err(e),
        Err(prov_core::storage::notes::NotesError::Schema(_)) => return Ok(0),
    };

    // Cutoff in ISO-8601 form. Comparing strings is safe for properly-formatted
    // ISO timestamps (lexicographic order matches chronological).
    let cutoff = match SystemTime::now().checked_sub(Duration::from_secs(
        u64::from(COMPACT_AGE_DAYS).saturating_mul(86_400),
    )) {
        Some(t) => system_time_to_iso8601(t),
        None => return Ok(0),
    };

    let mut compacted = 0_u32;
    for (sha, mut note) in entries {
        let newest = note
            .edits
            .iter()
            .map(|e| e.timestamp.as_str())
            .max()
            .unwrap_or("");
        if newest >= cutoff.as_str() || newest.is_empty() {
            continue;
        }
        let mut changed = false;
        for edit in &mut note.edits {
            if edit.preceding_turns_summary.is_some() {
                edit.preceding_turns_summary = None;
                changed = true;
            }
            if let Some(blob) = edit.original_blob_sha.as_deref() {
                if !blob_exists(git, blob) {
                    edit.original_blob_sha = None;
                    changed = true;
                }
            }
        }
        if !changed {
            continue;
        }
        if dry_run {
            println!("prov gc (dry-run, {ref_name}): would compact note {sha}");
            continue;
        }
        if let Err(e) = store.write(&sha, &note) {
            eprintln!("prov gc: rewriting compacted note {sha} on {ref_name} failed: {e}");
            continue;
        }
        compacted = compacted.saturating_add(1);
    }
    Ok(compacted)
}

fn blob_exists(git: &Git, blob_sha: &str) -> bool {
    git.run(["cat-file", "-e", blob_sha]).is_ok()
}

/// Return the newest mtime under `dir` (recursive shallow walk — staging dirs
/// are flat). Falls back to `None` when no readable file is found.
fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    for entry in std::fs::read_dir(dir).ok()? {
        let Ok(entry) = entry else { continue };
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            // Recurse one level to catch the `private/` subdir.
            if let Some(child) = newest_mtime(&entry.path()) {
                newest = Some(newest.map_or(child, |n| n.max(child)));
            }
            continue;
        }
        if let Ok(m) = meta.modified() {
            newest = Some(newest.map_or(m, |n| n.max(m)));
        }
    }
    newest
}

fn invalidate_cache(git: &Git) {
    let cache_path = git.git_dir().join(CACHE_FILENAME);
    if !cache_path.exists() {
        return;
    }
    // After cull/compact, the cache is best rebuilt wholesale on next read.
    // Clear the recorded notes-ref SHA so the resolver triggers a reindex on
    // first access; cheaper than walking every changed commit and deleting
    // its rows individually.
    if let Ok(cache) = Cache::open(&cache_path) {
        let _ = cache.set_recorded_notes_ref_sha(None);
    }
}

/// Format a `SystemTime` as `YYYY-MM-DDTHH:MM:SSZ` (ISO 8601 UTC).
fn system_time_to_iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (year, month, day, hour, minute, second) = prov_core::time::epoch_to_civil(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_formats_known_epoch() {
        assert_eq!(system_time_to_iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_formats_known_date() {
        // 2026-01-16T00:34:56Z (verified via `date -u -r 1768523696`).
        let secs = 1_768_523_696_u64;
        let t = UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(system_time_to_iso8601(t), "2026-01-16T00:34:56Z");
    }
}
