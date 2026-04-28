use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// `file:line` target whose original prompt should be replayed.
    pub target: String,
    /// Override the model recorded in the note.
    #[arg(long)]
    pub model: Option<String>,
    /// Walk `derived_from` to the original prompt rather than the most recent.
    #[arg(long)]
    pub root: bool,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("regenerate")
}
