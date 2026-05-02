//! `prov push [<remote>]` — push the local public notes ref to a remote.
//!
//! Shells `git push <remote> refs/notes/prompts:refs/notes/prompts`. The
//! pre-push hook (registered by `prov install`) fires before the push reaches
//! the wire and can block when an unredacted secret is detected; that gate is
//! what makes opt-in sharing safe by default.
//!
//! `refs/notes/prompts-private` is never pushed by this command, and the
//! pre-push gate also blocks any manual mapping that names the private ref
//! locally (see U8's hook handler).
//!
//! `--no-verify` is the documented escape hatch for users who need to bypass
//! the gate (e.g., a known false positive). When passed here, prov logs the
//! override to `<git-dir>/prov-staging/log` *before* invoking git, so an
//! audit trail exists even if the gate would have caught a real secret.

use anyhow::{anyhow, Context};
use clap::Parser;

use prov_core::git::{Git, GitError};
use prov_core::storage::staging::Staging;
use prov_core::time::now_iso8601;

const PUSH_REFSPEC: &str = "refs/notes/prompts:refs/notes/prompts";

#[derive(Parser, Debug)]
pub struct Args {
    /// Remote to push to (defaults to `origin`).
    pub remote: Option<String>,
    /// Skip the pre-push secret-scanning gate. Audit-logged before the push.
    #[arg(long)]
    pub no_verify: bool,
}

pub fn run(args: Args) -> anyhow::Result<()> {
    let remote = args.remote.unwrap_or_else(|| "origin".to_string());

    // Same fail-fast posture as `prov fetch`: a missing credential helper
    // should error, not block waiting on a TTY the caller can't drive.
    super::fetch::disable_git_terminal_prompt();

    let cwd = std::env::current_dir().context("could not read current directory")?;
    let git = Git::discover(&cwd).map_err(|e| match e {
        GitError::NotARepo => anyhow!("not in a git repo"),
        other => other.into(),
    })?;

    if args.no_verify {
        let staging = Staging::new(git.git_dir());
        let _ = staging.append_log(&format!(
            "{}: prov push {remote} --no-verify (pre-push gate bypassed)",
            now_iso8601()
        ));
    }

    let mut push_args: Vec<&str> = vec!["push"];
    if args.no_verify {
        push_args.push("--no-verify");
    }
    push_args.extend(["--", &remote, PUSH_REFSPEC]);

    git.run(push_args)
        .with_context(|| format!("git push {remote} {PUSH_REFSPEC}"))?;

    println!("prov push {remote}: refs/notes/prompts pushed");
    Ok(())
}
