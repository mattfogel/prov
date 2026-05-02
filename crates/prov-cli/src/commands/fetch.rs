//! `prov fetch [<remote>]` — pull `refs/notes/prompts` from a remote and merge
//! it into the local notes ref.
//!
//! The fetch step uses a tracking-ref refspec
//! (`refs/notes/prompts:refs/notes/origin/prompts`) so the remote's notes never
//! overwrite local writes. The merge step (`git notes merge`) honors the
//! `notes.mergeStrategy=manual` config that `prov install` sets, so divergent
//! notes surface as a merge in progress for `prov notes-resolve` (U10) rather
//! than silently picking a side.
//!
//! `refs/notes/prompts-private` is intentionally local-only and never fetched.

use anyhow::{anyhow, Context};
use clap::Parser;

use prov_core::git::{Git, GitError};
use prov_core::storage::NOTES_REF_PUBLIC;

#[derive(Parser, Debug)]
pub struct Args {
    /// Remote to fetch from (defaults to `origin`).
    pub remote: Option<String>,
}

const TRACKING_REF: &str = "refs/notes/origin/prompts";
const FETCH_REFSPEC: &str = "refs/notes/prompts:refs/notes/origin/prompts";

pub fn run(args: Args) -> anyhow::Result<()> {
    let remote = args.remote.unwrap_or_else(|| "origin".to_string());

    // Default git network ops to non-interactive: agents and CI can't answer
    // a TTY prompt, so a stuck credential helper would hang forever instead
    // of failing fast. Respect an existing override so a human running
    // `GIT_TERMINAL_PROMPT=1 prov fetch` for a one-off interactive auth
    // still gets the prompt.
    disable_git_terminal_prompt();

    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = Git::discover(&cwd).map_err(|e| match e {
        GitError::NotARepo => anyhow!("not in a git repo"),
        other => other.into(),
    })?;

    let before = note_count(&git, NOTES_REF_PUBLIC);

    // `--` separates options from positionals so a remote name beginning
    // with `-` is interpreted as a repository, not a flag.
    git.run(["fetch", "--", &remote, FETCH_REFSPEC])
        .with_context(|| format!("git fetch {remote} {FETCH_REFSPEC}"))?;

    // `git notes merge` is a no-op (and errors with "refusing to merge into
    // empty notes") when both sides are identical or the local ref is empty;
    // skip the merge attempt in those cases so a clean fetch reports cleanly.
    let local_sha = ref_sha(&git, NOTES_REF_PUBLIC);
    let tracking_sha = ref_sha(&git, TRACKING_REF);
    match (local_sha.as_deref(), tracking_sha.as_deref()) {
        (None, Some(tracking)) => {
            // First-time fetch: copy the tracking ref to the local ref directly.
            git.run(["update-ref", NOTES_REF_PUBLIC, tracking])
                .with_context(|| format!("update-ref {NOTES_REF_PUBLIC}"))?;
        }
        (Some(local), Some(tracking)) if local != tracking => {
            git.run(["notes", "--ref=prompts", "merge", TRACKING_REF])
                .with_context(|| {
                    "notes merge produced a conflict; run `prov notes-resolve` to finish"
                        .to_string()
                })?;
        }
        _ => {}
    }

    let after = note_count(&git, NOTES_REF_PUBLIC);
    let delta = after.saturating_sub(before);
    println!("prov fetch {remote}: {before} → {after} notes ({delta} new)");
    Ok(())
}

/// Count entries in a notes ref via `git notes list`. Returns 0 when the ref
/// does not exist yet.
fn note_count(git: &Git, ref_name: &str) -> usize {
    match git.capture(["notes", "--ref", ref_name, "list"]) {
        Ok(out) => out.lines().filter(|l| !l.trim().is_empty()).count(),
        Err(_) => 0,
    }
}

fn ref_sha(git: &Git, ref_name: &str) -> Option<String> {
    git.capture(["rev-parse", "--verify", "-q", ref_name])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Set `GIT_TERMINAL_PROMPT=0` for child git invocations unless the user has
/// explicitly opted into prompts. Shared between `prov fetch` and `prov push`
/// so the network commands fail fast instead of hanging on a missing
/// credential helper.
pub(crate) fn disable_git_terminal_prompt() {
    if std::env::var_os("GIT_TERMINAL_PROMPT").is_none() {
        std::env::set_var("GIT_TERMINAL_PROMPT", "0");
    }
}
