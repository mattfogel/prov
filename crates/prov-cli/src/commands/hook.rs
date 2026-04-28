use clap::{Parser, Subcommand};

/// Internal hook-event dispatch. Called by Claude Code hooks (`UserPromptSubmit`,
/// `PostToolUse`, `Stop`, `SessionStart`) and git hooks (`post-commit`,
/// `post-rewrite`, `pre-push`). Always exits 0 by design — Prov capture failures
/// must never block the user's editor or git operations.
#[derive(Parser, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub event: Event,
}

#[derive(Subcommand, Debug)]
pub enum Event {
    /// Claude Code: `UserPromptSubmit` — stage prompt + session metadata.
    UserPromptSubmit,
    /// Claude Code: `PostToolUse` matched on `Edit|Write|MultiEdit` — stage the edit.
    PostToolUse,
    /// Claude Code: `Stop` — mark current turn complete.
    Stop,
    /// Claude Code: `SessionStart` — capture model name for this session.
    SessionStart,
    /// Git: `post-commit` — flush staged edits into a note attached to HEAD.
    PostCommit,
    /// Git: `post-rewrite` — reattach notes after amend/rebase/squash.
    PostRewrite {
        /// `amend` or `rebase` — git passes this as the first arg to post-rewrite.
        kind: String,
    },
    /// Git: `pre-push` — scan notes refs for unredacted secrets before push.
    PrePush,
}

// Returns Result for consistency with the other `commands::*::run` signatures;
// Phase-1 default just exits 0. U3 fills in real dispatch.
#[allow(clippy::unnecessary_wraps)]
pub fn run(_args: Args) -> anyhow::Result<()> {
    Ok(())
}
