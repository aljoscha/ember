use clap::Args;

#[derive(Args)]
pub struct CpArgs {
    /// Source path (prefix with <vm-name>: for remote)
    pub src: String,

    /// Destination path (prefix with <vm-name>: for remote)
    pub dst: String,
}

pub fn run(_args: &CpArgs) -> anyhow::Result<()> {
    anyhow::bail!("crackling cp is not yet implemented")
}
