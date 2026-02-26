use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum VmCommand {
    /// Create a new VM from an image
    Create(CreateArgs),

    /// Start a stopped VM
    Start(StartArgs),

    /// Stop a running VM
    Stop(StopArgs),

    /// Pause a running VM
    Pause(PauseArgs),

    /// Resume a paused VM
    Resume(ResumeArgs),

    /// Delete a VM and its resources
    Delete(DeleteArgs),

    /// List all VMs
    List(ListArgs),

    /// Show detailed VM information
    Inspect(InspectArgs),

    /// Open an SSH session to a VM
    Ssh(SshArgs),
}

#[derive(Args)]
pub struct CreateArgs {
    /// VM name
    pub name: String,

    /// Base image reference
    #[arg(long)]
    pub image: String,

    /// Number of vCPUs
    #[arg(long, default_value = "1")]
    pub cpus: u32,

    /// Memory in MiB
    #[arg(long, default_value = "128")]
    pub memory: u32,

    /// Disk size in GiB
    #[arg(long, default_value = "1")]
    pub disk_size: u32,

    /// Path to custom kernel
    #[arg(long)]
    pub kernel: Option<PathBuf>,

    /// Network subnet
    #[arg(long)]
    pub network: Option<String>,

    /// VM config YAML file
    #[arg(long = "vm-config")]
    pub config: Option<PathBuf>,

    /// Don't start the VM after creation
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Args)]
pub struct StartArgs {
    /// VM name
    pub name: String,
}

#[derive(Args)]
pub struct StopArgs {
    /// VM name
    pub name: String,

    /// Force stop (SIGKILL)
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct PauseArgs {
    /// VM name
    pub name: String,
}

#[derive(Args)]
pub struct ResumeArgs {
    /// VM name
    pub name: String,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// VM name
    pub name: String,

    /// Force delete (kill if running)
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct ListArgs {
    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct InspectArgs {
    /// VM name
    pub name: String,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct SshArgs {
    /// VM name
    pub name: String,

    /// Command to run (everything after --)
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}

pub fn run(cmd: &VmCommand) -> anyhow::Result<()> {
    match cmd {
        VmCommand::Create(_) => anyhow::bail!("crackling vm create is not yet implemented"),
        VmCommand::Start(_) => anyhow::bail!("crackling vm start is not yet implemented"),
        VmCommand::Stop(_) => anyhow::bail!("crackling vm stop is not yet implemented"),
        VmCommand::Pause(_) => anyhow::bail!("crackling vm pause is not yet implemented"),
        VmCommand::Resume(_) => anyhow::bail!("crackling vm resume is not yet implemented"),
        VmCommand::Delete(_) => anyhow::bail!("crackling vm delete is not yet implemented"),
        VmCommand::List(_) => anyhow::bail!("crackling vm list is not yet implemented"),
        VmCommand::Inspect(_) => anyhow::bail!("crackling vm inspect is not yet implemented"),
        VmCommand::Ssh(_) => anyhow::bail!("crackling vm ssh is not yet implemented"),
    }
}
