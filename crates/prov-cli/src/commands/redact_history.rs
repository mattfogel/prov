//! `prov redact-history <pattern>` — retroactively scrub a regex pattern from
//! every note already on the local notes refs.
//!
//! Use case: after a session has shipped, the user discovers a class of secret
//! the redactor didn't catch (a new vendor's API token shape, a customer code
//! name, a private URL). Add the pattern to `.provignore` for future writes,
//! then run `prov redact-history` to walk every existing note and replace
//! matches in `prompt` and `preceding_turns_summary` with the marker
//! `[REDACTED:provignore-rule:cli]`.
//!
//! Operates on both `refs/notes/prompts` and `refs/notes/prompts-private`;
//! private notes are local-only but still benefit from the scrub in case they
//! get opted-in for push later.
//!
//! **Important caveat**: rewriting the local notes ref does NOT reach already-
//! distributed clones, forks, or teammate caches. The user MUST rotate the
//! underlying secret independently. We print this on stderr after a successful
//! rewrite so it's hard to miss.

use anyhow::{anyhow, Context};
use clap::Parser;
use regex::Regex;

use prov_core::git::{Git, GitError};
use prov_core::schema::Note;
use prov_core::storage::notes::NotesStore;
use prov_core::storage::sqlite::Cache;
use prov_core::storage::{NOTES_REF_PRIVATE, NOTES_REF_PUBLIC};

use super::common::CACHE_FILENAME;

/// Marker emitted in place of every CLI-supplied pattern hit. Mirrors the
/// `provignore-rule:<index>` shape produced by the runtime redactor; `cli`
/// distinguishes a retroactive `redact-history` rewrite from one of the
/// numbered rules in `.provignore`.
const CLI_MARKER: &str = "[REDACTED:provignore-rule:cli]";

#[derive(Parser, Debug)]
pub struct Args {
    /// Pattern (regex) to scrub retroactively across the notes refs.
    pub pattern: String,
}

#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> anyhow::Result<()> {
    // Validate the regex BEFORE touching either ref. The plan calls this out
    // explicitly: a bad pattern must error before any rewrite happens, so the
    // user doesn't end up with a partially-rewritten history.
    let re =
        Regex::new(&args.pattern).map_err(|e| anyhow!("invalid regex `{}`: {e}", args.pattern))?;

    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = Git::discover(&cwd).map_err(|e| match e {
        GitError::NotARepo => anyhow!("not in a git repo"),
        other => anyhow::Error::from(other),
    })?;

    let public = NotesStore::new(git.clone(), NOTES_REF_PUBLIC);
    let private = NotesStore::new(git.clone(), NOTES_REF_PRIVATE);

    let public_stats = rewrite_ref(&public, &re).context("rewriting public notes ref")?;
    let private_stats = rewrite_ref(&private, &re).context("rewriting private notes ref")?;

    let total_scanned = public_stats.scanned + private_stats.scanned;
    let total_rewritten = public_stats.rewritten + private_stats.rewritten;
    let total_secrets = public_stats.replacements + private_stats.replacements;

    if total_rewritten == 0 {
        println!("redact-history: 0 notes rewritten ({total_scanned} scanned)");
        return Ok(());
    }

    // Refresh the cache so subsequent reads (`prov log`, `prov search`) see
    // the scrubbed content. Failure is non-fatal — a stale cache shows the
    // pre-rewrite text until the user runs `prov reindex`, but the notes ref
    // is already authoritative.
    let cache_path = git.git_dir().join(CACHE_FILENAME);
    if cache_path.exists() {
        if let Ok(mut cache) = Cache::open(&cache_path) {
            let _ = cache.reindex_from(&public);
            let _ = cache.overlay_from(&private);
        }
    }

    println!(
        "redact-history: rewrote {total_rewritten} of {total_scanned} note(s); \
         redacted {total_secrets} match(es)"
    );
    eprintln!();
    eprintln!("Heads up:");
    eprintln!("  - Local notes refs are scrubbed, but already-pushed copies, forks, and");
    eprintln!("    teammate clones still hold the pre-rewrite content. Rotate the");
    eprintln!("    underlying secret independently.");
    eprintln!("  - Local reflog and unreferenced blob objects still hold the pre-rewrite");
    eprintln!("    content until pruned. Run:");
    eprintln!("        git reflog expire --expire=now --all");
    eprintln!("        git gc --prune=now");
    eprintln!("  - Teammates can re-sync after you re-push the rewritten ref:");
    eprintln!("        git fetch <remote> +refs/notes/prompts:refs/notes/prompts");
    eprintln!("        prov reindex");
    Ok(())
}

#[derive(Default)]
struct RewriteStats {
    scanned: u32,
    rewritten: u32,
    replacements: u32,
}

fn rewrite_ref(store: &NotesStore, pattern: &Regex) -> anyhow::Result<RewriteStats> {
    let entries = store.list().context("listing notes")?;
    let mut stats = RewriteStats::default();
    for (commit_sha, mut note) in entries {
        stats.scanned = stats.scanned.saturating_add(1);
        let mut hits_in_note: u32 = 0;
        for edit in &mut note.edits {
            hits_in_note = hits_in_note.saturating_add(scrub_in_place(&mut edit.prompt, pattern));
            if let Some(summary) = edit.preceding_turns_summary.as_mut() {
                hits_in_note = hits_in_note.saturating_add(scrub_in_place(summary, pattern));
            }
        }
        if hits_in_note > 0 {
            // Round-trip through `Note::new` to refresh the schema-version
            // stamp — defensive in case any older field shape is parsed in.
            let rewritten = Note::new(note.edits);
            store.write(&commit_sha, &rewritten)?;
            stats.rewritten = stats.rewritten.saturating_add(1);
            stats.replacements = stats.replacements.saturating_add(hits_in_note);
        }
    }
    Ok(stats)
}

/// Replace every `pattern` hit in `text` with the CLI marker. Returns the
/// number of replacements applied so the caller can keep a running total.
fn scrub_in_place(text: &mut String, pattern: &Regex) -> u32 {
    let count: u32 = pattern
        .find_iter(text)
        .count()
        .try_into()
        .unwrap_or(u32::MAX);
    if count == 0 {
        return 0;
    }
    *text = pattern.replace_all(text, CLI_MARKER).into_owned();
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_replaces_all_hits_and_counts_them() {
        let mut s = "the Acme launch and Acme rollout".to_string();
        let re = Regex::new("Acme").unwrap();
        let n = scrub_in_place(&mut s, &re);
        assert_eq!(n, 2);
        assert!(!s.contains("Acme"));
        assert!(s.contains(CLI_MARKER));
    }

    #[test]
    fn scrub_zero_hits_leaves_text_alone() {
        let mut s = "no matches here".to_string();
        let original = s.clone();
        let re = Regex::new("MISSING").unwrap();
        assert_eq!(scrub_in_place(&mut s, &re), 0);
        assert_eq!(s, original);
    }
}
