use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// Skip the interactive consent prompt for transcript-file access.
    #[arg(long)]
    pub yes: bool,
    /// Allow backfilling commits authored by a different user.email (loud warning).
    #[arg(long)]
    pub cross_author: bool,
    /// Surface every backfilled note regardless of confidence score.
    #[arg(long)]
    pub include_low_confidence: bool,
    /// Override the auto-discovered Claude Code transcript directory.
    #[arg(long, value_name = "PATH")]
    pub transcript_path: Option<String>,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("backfill")
}
