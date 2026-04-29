use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// File path, optionally with `:<line>` suffix.
    pub target: Option<String>,
    /// Show provenance history including superseded prompts.
    #[arg(long)]
    pub history: bool,
    /// Expand `preceding_turns_summary` into the full transcript.
    #[arg(long)]
    pub full: bool,
    /// Skip the lookup if the file has fewer than N lines or no existing notes.
    #[arg(long)]
    pub only_if_substantial: bool,
    /// Emit JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("log")
}
