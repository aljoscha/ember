use std::path::Path;

use clap::{Args, Subcommand};

use crate::backend::{Storage, StorageBackend};
use crate::config::GlobalConfig;
use crate::state::store::StateStore;
use crate::state::vm;

use super::vm::OutputFormat;

/// Reserved snapshot name used internally for image cloning (ZFS `@base`).
const RESERVED_SNAPSHOT_NAME: &str = "base";

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
        SnapshotCommand::Restore(args) => restore(args, state_dir),
        SnapshotCommand::List(args) => list(args, state_dir),
        SnapshotCommand::Delete(args) => delete(args, state_dir),
    }
}

/// Create a snapshot of a VM's disk.
///
/// The snapshot name must not conflict with the reserved `base` name
/// used for image cloning, and must not already exist.
fn create(args: &CreateArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);
    let _metadata = vm::load(&store, &args.vm_name)?;

    // Disallow the reserved snapshot name.
    if args.snapshot_name == RESERVED_SNAPSHOT_NAME {
        anyhow::bail!("snapshot name 'base' is reserved for image cloning");
    }

    // Check the snapshot doesn't already exist.
    let existing = storage.list_snapshots(&args.vm_name)?;
    if existing.iter().any(|s| s.name == args.snapshot_name) {
        anyhow::bail!(
            "snapshot '{}' already exists on vm '{}'",
            args.snapshot_name,
            args.vm_name
        );
    }

    storage.snapshot(&args.vm_name, &args.snapshot_name)?;

    println!(
        "Created snapshot '{}' of vm '{}'",
        args.snapshot_name, args.vm_name
    );
    Ok(())
}

/// List snapshots for a VM.
///
/// Shows all user-created snapshots, excluding internal snapshots.
/// Supports table and JSON output formats.
fn list(args: &ListArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);
    let _metadata = vm::load(&store, &args.vm_name)?;

    let snapshots = storage.list_snapshots(&args.vm_name)?;

    match args.format {
        OutputFormat::Json => {
            // Build a JSON-serializable list matching the backend's SnapshotInfo.
            let json_list: Vec<_> = snapshots
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "created_at": s.created_at,
                        "size": s.size,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_list)?);
        }
        OutputFormat::Table => {
            if snapshots.is_empty() {
                println!(
                    "No snapshots for vm '{}'. Create one with: ember snapshot create {} <name>",
                    args.vm_name, args.vm_name
                );
                return Ok(());
            }

            println!("{:<30} {:<24} {:>10}", "NAME", "CREATED", "SIZE");
            for snap in &snapshots {
                println!(
                    "{:<30} {:<24} {:>10}",
                    snap.name,
                    format_epoch(snap.created_at),
                    format_bytes(snap.size),
                );
            }
        }
    }

    Ok(())
}

/// Convert a Unix epoch timestamp to a human-readable UTC string.
///
/// Uses the same civil date algorithm as `state::vm::now_iso8601()`.
fn format_epoch(epoch: u64) -> String {
    let day_secs = epoch % 86400;
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;

    let days = epoch / 86400;
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {hour:02}:{min:02}:{sec:02} UTC")
}

use super::fmt::format_bytes_binary as format_bytes;

/// Delete a snapshot from a VM.
///
/// The reserved `base` snapshot cannot be deleted — it is used for image cloning.
fn delete(args: &DeleteArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);
    let _metadata = vm::load(&store, &args.vm_name)?;

    // Disallow deleting the reserved snapshot.
    if args.snapshot_name == RESERVED_SNAPSHOT_NAME {
        anyhow::bail!("snapshot 'base' is reserved for image cloning and cannot be deleted");
    }

    // Verify the snapshot exists.
    let existing = storage.list_snapshots(&args.vm_name)?;
    if !existing.iter().any(|s| s.name == args.snapshot_name) {
        anyhow::bail!(
            "snapshot '{}' does not exist on vm '{}'\n\
             Hint: list snapshots with: ember snapshot list {}",
            args.snapshot_name,
            args.vm_name,
            args.vm_name
        );
    }

    storage.delete_snapshot(&args.vm_name, &args.snapshot_name)?;

    println!(
        "Deleted snapshot '{}' from vm '{}'",
        args.snapshot_name, args.vm_name
    );
    Ok(())
}

/// Restore a VM's disk to a previously created snapshot.
///
/// The VM must be stopped — rolling back a disk that is in use by a running
/// hypervisor would corrupt it.
fn restore(args: &RestoreArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);
    let _metadata = vm::require_stopped(&store, &args.vm_name, "restoring a snapshot")?;

    // Verify the snapshot exists.
    let existing = storage.list_snapshots(&args.vm_name)?;
    if !existing.iter().any(|s| s.name == args.snapshot_name) {
        anyhow::bail!(
            "snapshot '{}' does not exist on vm '{}'\n\
             Hint: list snapshots with: ember snapshot list {}",
            args.snapshot_name,
            args.vm_name,
            args.vm_name
        );
    }

    storage.restore_snapshot(&args.vm_name, &args.snapshot_name)?;

    println!(
        "Restored vm '{}' to snapshot '{}'",
        args.vm_name, args.snapshot_name
    );
    Ok(())
}
