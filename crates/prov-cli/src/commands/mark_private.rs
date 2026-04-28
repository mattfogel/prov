use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// Commit SHA whose note should move to the local-only private ref.
    pub commit: String,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("mark-private")
}
