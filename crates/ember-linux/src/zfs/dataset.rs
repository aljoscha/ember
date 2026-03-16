//! ZFS dataset operations via the `zfs` CLI.
//!
//! Datasets are filesystem-type containers (not zvols) used as parent
//! namespaces — e.g. `<pool>/images` and `<pool>/vms`.

use std::process::Command;

use ember_core::error::{Error, Result};

/// Check whether a ZFS dataset exists.
pub fn exists(dataset: &str) -> Result<bool> {
    let output = Command::new("zfs")
        .args(["list", "-H", "-o", "name", dataset])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs list".to_string(),
            source: e,
        })?;

    Ok(output.status.success())
}

/// Create a ZFS dataset (filesystem type).
///
/// Sets `mountpoint=none` since ember uses datasets only as parent
/// namespaces for zvols, not as mounted filesystems.
///
/// Creates parent datasets as needed (`-p` flag).
pub fn create(dataset: &str) -> Result<()> {
    let output = Command::new("zfs")
        .args(["create", "-p", "-o", "mountpoint=none", dataset])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs create".to_string(),
            source: e,
        })?;

    Error::check_command("zfs create", output)?;
    Ok(())
}
