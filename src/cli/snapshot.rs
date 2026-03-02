use std::path::Path;

use clap::{Args, Subcommand};

use crate::state::store::StateStore;
use crate::state::vm;
use crate::zfs;

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

pub fn run(cmd: &SnapshotCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        SnapshotCommand::Create(args) => create(args, state_dir),
        SnapshotCommand::Restore(_) => {
            anyhow::bail!("ember snapshot restore is not yet implemented")
        }
        SnapshotCommand::List(_) => {
            anyhow::bail!("ember snapshot list is not yet implemented")
        }
        SnapshotCommand::Delete(_) => {
            anyhow::bail!("ember snapshot delete is not yet implemented")
        }
    }
}

/// Create a ZFS snapshot of a VM's zvol.
///
/// The snapshot name must not conflict with the reserved `@base` snapshot
/// used for image cloning, and must not already exist.
fn create(args: &CreateArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let metadata = vm::load(&store, &args.vm_name)?;

    // Disallow the reserved @base snapshot name.
    if args.snapshot_name == "base" {
        anyhow::bail!("snapshot name 'base' is reserved for image cloning");
    }

    // Check the snapshot doesn't already exist.
    if zfs::snapshot::exists(&metadata.zvol_path, &args.snapshot_name)? {
        anyhow::bail!(
            "snapshot '{}' already exists on vm '{}'",
            args.snapshot_name,
            args.vm_name
        );
    }

    zfs::snapshot::create(&metadata.zvol_path, &args.snapshot_name)?;

    println!(
        "Created snapshot '{}' of vm '{}'",
        args.snapshot_name, args.vm_name
    );
    Ok(())
}
