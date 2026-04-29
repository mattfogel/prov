use clap::Parser;

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

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("pr-timeline")
}
