use clap::Args;

#[derive(Args)]
pub struct InitArgs {
    /// ZFS pool name
    #[arg(long, default_value = "crackling")]
    pub pool: String,

    /// Block device for pool creation
    #[arg(long)]
    pub device: Option<String>,

    /// Dataset name within the pool
    #[arg(long, default_value = "crackling")]
    pub dataset: String,

    /// URL to download the kernel from
    #[arg(long)]
    pub kernel_url: Option<String>,
}

pub fn run(_args: &InitArgs) -> anyhow::Result<()> {
    anyhow::bail!("crackling init is not yet implemented")
}
