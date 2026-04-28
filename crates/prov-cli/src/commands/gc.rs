use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// Also rewrite notes older than the compaction threshold (90d) to drop bulky fields.
    #[arg(long)]
    pub compact: bool,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("gc")
}
