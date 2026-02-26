use clap::Args;

#[derive(Args)]
pub struct ExecArgs {
    /// VM name
    pub vm_name: String,

    /// User to run the command as
    #[arg(long, default_value = "root")]
    pub user: String,

    /// Command to execute (everything after --)
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

pub fn run(_args: &ExecArgs) -> anyhow::Result<()> {
    anyhow::bail!("crackling exec is not yet implemented")
}
