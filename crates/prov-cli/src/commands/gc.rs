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
use serde::Serialize;

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
    /// Emit JSON instead of human-readable output. The Skill (U12) and other
    /// agents depend on this to parse housekeeping results without scraping.
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct GcJson {
    culled_public: Vec<String>,
    culled_private: Vec<String>,
    pruned_sessions: Vec<String>,
    compacted: Vec<String>,
    dry_run: bool,
    prov_version: &'static str,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = Git::discover(&cwd).map_err(|e| match e {
        GitError::NotARepo => anyhow!("not in a git repo"),
        other => anyhow::Error::from(other),
    })?;

    let culled_public = cull_unreachable(&git, NOTES_REF_PUBLIC, args.dry_run)
        .with_context(|| format!("culling unreachable notes on {NOTES_REF_PUBLIC}"))?;
    let culled_private = cull_unreachable(&git, NOTES_REF_PRIVATE, args.dry_run)
        .with_context(|| format!("culling unreachable notes on {NOTES_REF_PRIVATE}"))?;

    let pruned_sessions = prune_staging(&git, args.staging_ttl_days, args.dry_run);

    let mut compacted: Vec<String> = Vec::new();
    if args.compact {
        for ref_name in [NOTES_REF_PUBLIC, NOTES_REF_PRIVATE] {
            compacted.extend(
                compact_old_notes(&git, ref_name, args.dry_run)
                    .with_context(|| format!("compacting old notes on {ref_name}"))?,
            );
        }
    }

    let total_changed = culled_public.len() + culled_private.len() + compacted.len();
    if !args.dry_run && total_changed > 0 {
        invalidate_cache(&git);
    }

    if args.json {
        let payload = GcJson {
            culled_public,
            culled_private,
            pruned_sessions,
            compacted,
            dry_run: args.dry_run,
            prov_version: env!("CARGO_PKG_VERSION"),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let label = if args.dry_run { " (dry-run)" } else { "" };
    println!(
        "prov gc{label}: culled {} public + {} private unreachable note(s); \
         pruned {} stale staging session(s); \
         compacted {} note(s)",
        culled_public.len(),
        culled_private.len(),
        pruned_sessions.len(),
        compacted.len(),
    );
    if !args.dry_run && (culled_public.len() + culled_private.len()) > 0 {
        println!(
            "  note: reflog entries and unreferenced note blobs are preserved. \
             To fully reclaim space locally: \
             `git reflog expire --expire=now --all && git gc --prune=now`. \
             Already-pushed copies on remotes are NOT scrubbed — re-push the notes ref."
        );
    }
    Ok(())
}

/// Return true if `commit_sha` is reachable from any ref or from HEAD.
///
/// Three-step check: existence guard (`cat-file -e`), then `for-each-ref
/// --contains` for ref-reachable commits, then HEAD reachability via
/// `merge-base --is-ancestor` to catch detached-HEAD WIP that no branch
/// points at.
///
/// Reflog-only-reachable commits (e.g., a deleted-branch tip still pinned
/// by HEAD's reflog) are intentionally treated as unreachable per the
/// strict ref-reachability policy in the plan — git's own gc will expire
/// the reflog entries on its own schedule, and prov's tracking should
/// match the visible-refs view. Without HEAD coverage, `prov gc` would
/// cull notes attached to in-progress detached-HEAD work — `git
/// for-each-ref` does not consider HEAD as a starting point.
fn is_reachable(git: &Git, commit_sha: &str) -> bool {
    if git.run(["cat-file", "-e", commit_sha]).is_err() {
        return false;
    }
    if let Ok(s) = git.capture(["for-each-ref", "--contains", commit_sha]) {
        if !s.trim().is_empty() {
            return true;
        }
    }
    git.run(["merge-base", "--is-ancestor", commit_sha, "HEAD"])
        .is_ok()
}

/// Returns the SHAs of culled (or, in dry-run, would-be-culled) notes.
fn cull_unreachable(git: &Git, ref_name: &str, dry_run: bool) -> Result<Vec<String>, GitError> {
    let store = NotesStore::new(git.clone(), ref_name);
    let entries = match store.list() {
        Ok(v) => v,
        Err(prov_core::storage::notes::NotesError::Git(e)) => return Err(e),
        Err(prov_core::storage::notes::NotesError::Schema(_)) => {
            // A note we can't parse isn't reachable for our purposes; leave it
            // alone — the user can re-run after upgrading prov.
            return Ok(Vec::new());
        }
    };

    let mut culled: Vec<String> = Vec::new();
    for (sha, _note) in entries {
        if is_reachable(git, &sha) {
            continue;
        }
        if dry_run {
            println!("prov gc (dry-run, {ref_name}): would cull note for unreachable {sha}");
            culled.push(sha);
            continue;
        }
        if let Err(e) = store.remove(&sha) {
            eprintln!("prov gc: removing note for {sha} on {ref_name} failed: {e}");
            continue;
        }
        culled.push(sha);
    }
    Ok(culled)
}

/// Walk `<git-dir>/prov-staging/<session>/` and remove session dirs whose most
/// recent file mtime is older than `ttl_days`. Returns the session ids of
/// pruned (or, in dry-run, would-be-pruned) sessions. Defensive: a session
/// that's currently being written to (modified within TTL) is preserved.
fn prune_staging(git: &Git, ttl_days: u32, dry_run: bool) -> Vec<String> {
    let staging_root = git.git_dir().join(STAGING_DIRNAME);
    if !staging_root.exists() {
        return Vec::new();
    }
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(
        u64::from(ttl_days).saturating_mul(86_400),
    )) else {
        return Vec::new();
    };
    let staging = Staging::new(git.git_dir());
    let sessions = staging.list_sessions().unwrap_or_default();
    let mut pruned: Vec<String> = Vec::new();
    for sid in sessions {
        let dir = staging.session_dir(&sid, false);
        // An empty/unreadable session dir has no mtime to anchor on. Treat
        // it as maximally stale (UNIX_EPOCH) so it falls past any cutoff and
        // gets pruned, instead of falling back to `now()` and surviving forever.
        let last_mtime = newest_mtime(&dir).unwrap_or(UNIX_EPOCH);
        if last_mtime >= cutoff {
            continue;
        }
        if dry_run {
            println!(
                "prov gc (dry-run): would prune staging session {} (idle ≥ {ttl_days}d)",
                sid.as_str()
            );
            pruned.push(sid.as_str().to_string());
            continue;
        }
        if let Err(e) = staging.remove_session(&sid) {
            eprintln!(
                "prov gc: removing staging session {} failed: {e}",
                sid.as_str()
            );
            continue;
        }
        pruned.push(sid.as_str().to_string());
    }
    pruned
}

/// Compact notes older than [`COMPACT_AGE_DAYS`] by clearing
/// `preceding_turns_summary` and unreachable `original_blob_sha` fields.
/// Returns the SHAs of compacted (or, in dry-run, would-be-compacted) notes.
/// "Older" is measured by the note's most recent edit `timestamp` (ISO-8601
/// strings sort lexicographically when zero-padded). Notes whose edits all
/// have empty/unparseable timestamps are skipped.
fn compact_old_notes(git: &Git, ref_name: &str, dry_run: bool) -> Result<Vec<String>, GitError> {
    let store = NotesStore::new(git.clone(), ref_name);
    let entries = match store.list() {
        Ok(v) => v,
        Err(prov_core::storage::notes::NotesError::Git(e)) => return Err(e),
        Err(prov_core::storage::notes::NotesError::Schema(_)) => return Ok(Vec::new()),
    };

    // Cutoff in ISO-8601 form. Comparing strings is safe for properly-formatted
    // ISO timestamps (lexicographic order matches chronological).
    let cutoff = match SystemTime::now().checked_sub(Duration::from_secs(
        u64::from(COMPACT_AGE_DAYS).saturating_mul(86_400),
    )) {
        Some(t) => system_time_to_iso8601(t),
        None => return Ok(Vec::new()),
    };

    let mut compacted: Vec<String> = Vec::new();
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
            compacted.push(sha);
            continue;
        }
        if let Err(e) = store.write(&sha, &note) {
            eprintln!("prov gc: rewriting compacted note {sha} on {ref_name} failed: {e}");
            continue;
        }
        compacted.push(sha);
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
