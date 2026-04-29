use clap::{Parser, Subcommand};

mod commands;
mod render;

#[derive(Parser)]
#[command(
    name = "prov",
    version,
    about = "Prompt provenance for AI-generated code",
    long_about = "Prov captures the prompt-and-conversation context behind AI-generated code, \
                  attaches it to commits via git notes, and exposes it to humans (CLI), agents \
                  (Claude Code Skill), and reviewers (GitHub Action). Notes are local-only by \
                  default; opt in to team sharing per-repo via `prov sync enable`."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show provenance for a file or specific line.
    Log(commands::log::Args),
    /// Search prompts across the repo's history.
    Search(commands::search::Args),
    /// Render the PR intent timeline locally (also called by the GitHub Action).
    PrTimeline(commands::pr_timeline::Args),
    /// Rebuild the `SQLite` cache from the notes ref.
    Reindex(commands::reindex::Args),
    /// Install prov into the current repo (hooks, git config, plugin, optional sync enablement).
    Install(commands::install::Args),
    /// Remove prov from the current repo.
    Uninstall(commands::uninstall::Args),
    /// Fetch provenance notes from a remote (when sync is enabled).
    Fetch(commands::fetch::Args),
    /// Push provenance notes to a remote (when sync is enabled).
    Push(commands::push::Args),
    /// Resolve a notes-ref merge conflict via JSON-aware union.
    NotesResolve(commands::notes_resolve::Args),
    /// Mark an existing commit's note as private (move to the local-only ref).
    MarkPrivate(commands::mark_private::Args),
    /// Rewrite the notes ref to retroactively scrub a newly discovered secret pattern.
    RedactHistory(commands::redact_history::Args),
    /// Walk the reflog to reattach orphaned notes after rebase/amend bypassed Prov hooks.
    Repair(commands::repair::Args),
    /// Cull notes for unreachable commits and prune stale staging entries.
    Gc(commands::gc::Args),
    /// Re-run the original prompt against a chosen model and diff against the stored output.
    Regenerate(commands::regenerate::Args),
    /// Best-effort historical capture from stored Claude Code session transcripts.
    Backfill(commands::backfill::Args),
    /// Internal: hook-event dispatch (called by Claude Code hooks and git hooks).
    #[command(hide = true)]
    Hook(commands::hook::Args),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Log(args) => commands::log::run(args),
        Command::Search(args) => commands::search::run(args),
        Command::PrTimeline(args) => commands::pr_timeline::run(args),
        Command::Reindex(args) => commands::reindex::run(args),
        Command::Install(args) => commands::install::run(args),
        Command::Uninstall(args) => commands::uninstall::run(args),
        Command::Fetch(args) => commands::fetch::run(args),
        Command::Push(args) => commands::push::run(args),
        Command::NotesResolve(args) => commands::notes_resolve::run(args),
        Command::MarkPrivate(args) => commands::mark_private::run(args),
        Command::RedactHistory(args) => commands::redact_history::run(args),
        Command::Repair(args) => commands::repair::run(args),
        Command::Gc(args) => commands::gc::run(args),
        Command::Regenerate(args) => commands::regenerate::run(args),
        Command::Backfill(args) => commands::backfill::run(args),
        Command::Hook(args) => commands::hook::run(args),
    }
}
