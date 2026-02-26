use clap::{Args, Subcommand};

use super::vm::OutputFormat;

#[derive(Subcommand)]
pub enum SnapshotCommand {
    /// Create a snapshot of a VM
    Create(CreateArgs),

    /// Restore a VM to a snapshot
    Restore(RestoreArgs),

    /// List snapshots for a VM
    List(ListArgs),

    /// Delete a VM snapshot
    Delete(DeleteArgs),
}

#[derive(Args)]
pub struct CreateArgs {
    /// VM name
    pub vm_name: String,

    /// Snapshot name
    pub snapshot_name: String,
}

#[derive(Args)]
pub struct RestoreArgs {
    /// VM name
    pub vm_name: String,

    /// Snapshot name
    pub snapshot_name: String,
}

#[derive(Args)]
pub struct ListArgs {
    /// VM name
    pub vm_name: String,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// VM name
    pub vm_name: String,

    /// Snapshot name
    pub snapshot_name: String,
}

pub fn run(cmd: &SnapshotCommand) -> anyhow::Result<()> {
    match cmd {
        SnapshotCommand::Create(_) => {
            anyhow::bail!("crackling snapshot create is not yet implemented")
        }
        SnapshotCommand::Restore(_) => {
            anyhow::bail!("crackling snapshot restore is not yet implemented")
        }
        SnapshotCommand::List(_) => {
            anyhow::bail!("crackling snapshot list is not yet implemented")
        }
        SnapshotCommand::Delete(_) => {
            anyhow::bail!("crackling snapshot delete is not yet implemented")
        }
    }
}
