use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// Print Claude Code marketplace install instructions instead of writing project-scope config.
    #[arg(long)]
    pub plugin: bool,
    /// Enable team-mode sync at install time (configures fetch/push refspecs and pre-push gate
    /// for the named remote). Defaults to local-only — sync is opt-in per-repo.
    #[arg(long, value_name = "REMOTE")]
    pub enable_push: Option<String>,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("install")
}
