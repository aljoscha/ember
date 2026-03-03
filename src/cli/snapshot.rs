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
        SnapshotCommand::Restore(args) => restore(args, state_dir),
        SnapshotCommand::List(args) => list(args, state_dir),
        SnapshotCommand::Delete(args) => delete(args, state_dir),
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
    if args.snapshot_name == zfs::BASE_SNAPSHOT_NAME {
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

/// List ZFS snapshots for a VM.
///
/// Shows all user-created snapshots on the VM's zvol, excluding the
/// internal `@base` snapshot used for image cloning. Supports table
/// and JSON output formats.
fn list(args: &ListArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let metadata = vm::load(&store, &args.vm_name)?;

    let snapshots: Vec<_> = zfs::snapshot::list(&metadata.zvol_path)?
        .into_iter()
        .filter(|s| s.short_name != zfs::BASE_SNAPSHOT_NAME)
        .collect();

    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&snapshots)?);
        }
        OutputFormat::Table => {
            if snapshots.is_empty() {
                println!(
                    "No snapshots for vm '{}'. Create one with: ember snapshot create {} <name>",
                    args.vm_name, args.vm_name
                );
                return Ok(());
            }

            println!(
                "{:<30} {:<24} {:>10} {:>10}",
                "NAME", "CREATED", "USED", "REFER"
            );
            for snap in &snapshots {
                println!(
                    "{:<30} {:<24} {:>10} {:>10}",
                    snap.short_name,
                    format_epoch(snap.creation),
                    format_bytes(snap.used),
                    format_bytes(snap.referenced),
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

/// Format a byte count as a human-readable string (KiB, MiB, GiB).
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Delete a ZFS snapshot from a VM's zvol.
///
/// The reserved `@base` snapshot cannot be deleted — it is used for image
/// cloning. ZFS will return an error if the snapshot has dependent clones.
fn delete(args: &DeleteArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let metadata = vm::load(&store, &args.vm_name)?;

    // Disallow deleting the reserved @base snapshot.
    if args.snapshot_name == zfs::BASE_SNAPSHOT_NAME {
        anyhow::bail!("snapshot 'base' is reserved for image cloning and cannot be deleted");
    }

    // Verify the snapshot exists.
    if !zfs::snapshot::exists(&metadata.zvol_path, &args.snapshot_name)? {
        anyhow::bail!(
            "snapshot '{}' does not exist on vm '{}'\n\
             Hint: list snapshots with: ember snapshot list {}",
            args.snapshot_name,
            args.vm_name,
            args.vm_name
        );
    }

    zfs::snapshot::destroy(&metadata.zvol_path, &args.snapshot_name)?;

    println!(
        "Deleted snapshot '{}' from vm '{}'",
        args.snapshot_name, args.vm_name
    );
    Ok(())
}

/// Restore a VM's zvol to a previously created snapshot.
///
/// The VM must be stopped (or never started) — rolling back a zvol that is
/// in use by a running Firecracker process would corrupt it. Uses
/// `zfs rollback -r`, which destroys any snapshots newer than the target.
fn restore(args: &RestoreArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let metadata = vm::require_stopped(&store, &args.vm_name, "restoring a snapshot")?;

    // Verify the snapshot exists.
    if !zfs::snapshot::exists(&metadata.zvol_path, &args.snapshot_name)? {
        anyhow::bail!(
            "snapshot '{}' does not exist on vm '{}'\n\
             Hint: list snapshots with: ember snapshot list {}",
            args.snapshot_name,
            args.vm_name,
            args.vm_name
        );
    }

    zfs::snapshot::rollback(&metadata.zvol_path, &args.snapshot_name)?;

    println!(
        "Restored vm '{}' to snapshot '{}'",
        args.vm_name, args.snapshot_name
    );
    Ok(())
}
