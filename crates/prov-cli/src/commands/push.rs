use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// Remote to push to (defaults to `origin`).
    pub remote: Option<String>,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("push")
}
