//! ZFS snapshot operations via the `zfs` CLI.
//!
//! Snapshots are point-in-time copies of datasets or zvols. Crackling
//! uses them for:
//!   - `@base` snapshots on image zvols (clone source for VMs)
//!   - User-created snapshots on VM zvols (Phase 6)

use std::process::Command;

use crate::error::{Error, Result};

/// Create a ZFS snapshot.
///
/// `dataset` is the full dataset/zvol path (e.g. `tank/crackling/images/alpine-latest`)
/// and `name` is the snapshot name (e.g. `base`), producing
/// `tank/crackling/images/alpine-latest@base`.
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

#[cfg(test)]
mod tests {
    #[test]
    fn snapshot_name_format() {
        let dataset = "tank/crackling/images/library-alpine-latest";
        let name = "base";
        let snapshot = format!("{dataset}@{name}");
        assert_eq!(
            snapshot,
            "tank/crackling/images/library-alpine-latest@base"
        );
    }
}
