use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// Search query (matched against prompt text via `SQLite` FTS5).
    pub query: String,
    /// Emit JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("search")
}
