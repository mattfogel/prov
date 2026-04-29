use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// Days of reflog history to walk (default: 14).
    #[arg(long, default_value_t = 14)]
    pub days: u32,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("repair")
}
