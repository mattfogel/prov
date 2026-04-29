use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("reindex")
}
