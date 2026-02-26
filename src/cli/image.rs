use clap::{Args, Subcommand};

use super::vm::OutputFormat;

#[derive(Subcommand)]
pub enum ImageCommand {
    /// Pull an OCI image from a registry
    Pull(PullArgs),

    /// List locally available images
    List(ListArgs),

    /// Delete a local image
    Delete(DeleteArgs),

    /// Show detailed image information
    Inspect(InspectArgs),
}

#[derive(Args)]
pub struct PullArgs {
    /// Image reference (e.g. docker.io/library/ubuntu:22.04)
    pub reference: String,
}

#[derive(Args)]
pub struct ListArgs {
    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// Image name
    pub name: String,
}

#[derive(Args)]
pub struct InspectArgs {
    /// Image name
    pub name: String,
}

pub fn run(cmd: &ImageCommand) -> anyhow::Result<()> {
    match cmd {
        ImageCommand::Pull(_) => anyhow::bail!("crackling image pull is not yet implemented"),
        ImageCommand::List(_) => anyhow::bail!("crackling image list is not yet implemented"),
        ImageCommand::Delete(_) => anyhow::bail!("crackling image delete is not yet implemented"),
        ImageCommand::Inspect(_) => anyhow::bail!("crackling image inspect is not yet implemented"),
    }
}
