use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// Pattern (regex) to scrub retroactively across the notes ref.
    pub pattern: String,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("redact-history")
}
