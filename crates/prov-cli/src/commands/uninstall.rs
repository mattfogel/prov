use clap::Parser;

#[derive(Parser, Debug)]
pub struct Args {
    /// Also delete `.git/prov.db` and `.git/prov-staging/`. Notes ref is preserved.
    #[arg(long)]
    pub purge: bool,
}

pub fn run(_args: Args) -> anyhow::Result<()> {
    super::unimplemented_stub("uninstall")
}
