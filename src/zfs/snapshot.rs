//! ZFS snapshot operations via the `zfs` CLI.
//!
//! Snapshots are point-in-time copies of datasets or zvols. Ember
//! uses them for:
//!   - `@base` snapshots on image zvols (clone source for VMs)
//!   - User-created snapshots on VM zvols (Phase 6)

use std::process::Command;

use serde::Serialize;

use crate::error::{Error, Result};

/// Summary information about a ZFS snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    /// Full snapshot name including dataset (e.g. `tank/ember/vms/myvm@snap1`).
    pub name: String,
    /// Just the snapshot suffix after `@` (e.g. `snap1`).
    pub short_name: String,
    /// Bytes used exclusively by this snapshot.
    pub used: u64,
    /// Bytes referenced by this snapshot.
    pub referenced: u64,
    /// Creation timestamp as reported by ZFS (Unix epoch seconds).
    pub creation: u64,
}

/// Create a ZFS snapshot.
///
/// `dataset` is the full dataset/zvol path (e.g. `tank/ember/images/alpine-latest`)
/// and `name` is the snapshot name (e.g. `base`), producing
/// `tank/ember/images/alpine-latest@base`.
pub fn create(dataset: &str, name: &str) -> Result<()> {
    let snapshot = format!("{dataset}@{name}");

    let output = Command::new("zfs")
        .args(["snapshot", &snapshot])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs snapshot".to_string(),
            source: e,
        })?;

    Error::check_command("zfs snapshot", output)?;
    Ok(())
}

/// Check whether a ZFS snapshot exists.
pub fn exists(dataset: &str, name: &str) -> Result<bool> {
    let snapshot = format!("{dataset}@{name}");

    let output = Command::new("zfs")
        .args(["list", "-H", "-t", "snapshot", "-o", "name", &snapshot])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs list".to_string(),
            source: e,
        })?;

    Ok(output.status.success())
}

/// Roll back a dataset/zvol to a snapshot.
///
/// This discards all changes made after the snapshot was taken.
/// The dataset must not be in use (e.g. the VM must be stopped).
///
/// Uses `-r` to destroy any more recent snapshots if necessary.
pub fn rollback(dataset: &str, name: &str) -> Result<()> {
    let snapshot = format!("{dataset}@{name}");

    let output = Command::new("zfs")
        .args(["rollback", "-r", &snapshot])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs rollback".to_string(),
            source: e,
        })?;

    Error::check_command("zfs rollback", output)?;
    Ok(())
}

/// List all snapshots under a dataset/zvol.
///
/// Returns snapshots sorted by creation time (oldest first, as ZFS reports
/// them). The `@base` snapshot used for image cloning is included in the
/// results — callers can filter it out if needed.
pub fn list(dataset: &str) -> Result<Vec<SnapshotInfo>> {
    let output = Command::new("zfs")
        .args([
            "list", "-Hp", "-r", "-t", "snapshot",
            "-o", "name,used,refer,creation",
            dataset,
        ])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs list".to_string(),
            source: e,
        })?;

    // If the dataset doesn't exist or has no snapshots, zfs list exits
    // non-zero. Return an empty list rather than an error.
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut snapshots = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 4 {
            continue;
        }

        let full_name = fields[0];
        let short_name = full_name
            .rsplit_once('@')
            .map(|(_, s)| s.to_string())
            .unwrap_or_default();

        snapshots.push(SnapshotInfo {
            name: full_name.to_string(),
            short_name,
            used: super::parse_u64(fields[1], "used")?,
            referenced: super::parse_u64(fields[2], "referenced")?,
            creation: super::parse_u64(fields[3], "creation")?,
        });
    }

    Ok(snapshots)
}

/// Destroy a ZFS snapshot.
///
/// Removes the named snapshot from the dataset. This operation fails if
/// the snapshot has dependent clones (use `zfs::volume::destroy` with
/// `recursive: true` to remove a zvol and all its snapshots).
pub fn destroy(dataset: &str, name: &str) -> Result<()> {
    let snapshot = format!("{dataset}@{name}");

    let output = Command::new("zfs")
        .args(["destroy", &snapshot])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs destroy".to_string(),
            source: e,
        })?;

    Error::check_command("zfs destroy", output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn snapshot_name_format() {
        let dataset = "tank/ember/images/library-alpine-latest";
        let name = "base";
        let snapshot = format!("{dataset}@{name}");
        assert_eq!(
            snapshot,
            "tank/ember/images/library-alpine-latest@base"
        );
    }

    #[test]
    fn parse_snapshot_info_line() {
        // Simulate `zfs list -Hp -t snapshot -o name,used,refer,creation`
        let line = "tank/ember/vms/myvm@snap1\t65536\t1048576\t1709337600";
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0], "tank/ember/vms/myvm@snap1");
        assert_eq!(fields[1].parse::<u64>().unwrap(), 65536);
        assert_eq!(fields[2].parse::<u64>().unwrap(), 1048576);
        assert_eq!(fields[3].parse::<u64>().unwrap(), 1709337600);

        let short_name = fields[0]
            .rsplit_once('@')
            .map(|(_, s)| s.to_string())
            .unwrap_or_default();
        assert_eq!(short_name, "snap1");
    }

    #[test]
    fn parse_list_output_multiple_snapshots() {
        let output = "tank/ember/vms/myvm@snap1\t65536\t1048576\t1709337600\ntank/ember/vms/myvm@snap2\t131072\t2097152\t1709424000\n";

        let snapshots: Vec<Vec<&str>> = output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split('\t').collect())
            .collect();

        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0][0], "tank/ember/vms/myvm@snap1");
        assert_eq!(snapshots[1][0], "tank/ember/vms/myvm@snap2");
    }

    #[test]
    fn short_name_extraction() {
        let cases = vec![
            ("tank/ember/vms/myvm@snap1", "snap1"),
            ("tank/ember/vms/myvm@base", "base"),
            ("pool/dataset@backup-2024-03-01", "backup-2024-03-01"),
        ];

        for (full, expected) in cases {
            let short = full
                .rsplit_once('@')
                .map(|(_, s)| s.to_string())
                .unwrap_or_default();
            assert_eq!(short, expected, "failed for input: {full}");
        }
    }
}
