//! ZFS volume (zvol) operations via the `zfs` CLI.
//!
//! Zvols are block devices backed by ZFS. Ember uses them as root drives
//! for Firecracker VMs — image zvols under `<pool>/images/` and per-VM clones
//! under `<pool>/vms/`.

use std::path::PathBuf;
use std::process::Command;

use ember_core::error::{Error, Result};

/// Check whether a ZFS zvol exists.
pub fn exists(zvol: &str) -> Result<bool> {
    let output = Command::new("zfs")
        .args(["list", "-H", "-t", "volume", "-o", "name", zvol])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs list".to_string(),
            source: e,
        })?;

    Ok(output.status.success())
}

/// Create a ZFS zvol with the given size in mebibytes.
///
/// The zvol appears as a block device at `/dev/zvol/<name>` once created.
/// Parents are created as needed (`-p` flag).
pub fn create(zvol: &str, size_mib: u64) -> Result<()> {
    let size_arg = format!("{size_mib}M");

    let output = Command::new("zfs")
        .args(["create", "-p", "-V", &size_arg, zvol])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs create".to_string(),
            source: e,
        })?;

    Error::check_command("zfs create", output)?;
    Ok(())
}

/// Destroy a ZFS zvol.
///
/// With `recursive: true`, also destroys all snapshots and clones under
/// the zvol (`-r` flag).
pub fn destroy(zvol: &str, recursive: bool) -> Result<()> {
    super::destroy(zvol, recursive)
}

/// Clone a ZFS snapshot to create a new zvol.
///
/// This is an instant copy-on-write clone — the new zvol shares blocks
/// with the snapshot and only diverges as writes occur.
///
/// `snapshot` is the full snapshot path (e.g. `tank/ember/images/alpine@base`).
/// `new_zvol` is the destination zvol (e.g. `tank/ember/vms/myvm`).
pub fn clone(snapshot: &str, new_zvol: &str) -> Result<()> {
    let output = Command::new("zfs")
        .args(["clone", "-p", snapshot, new_zvol])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs clone".to_string(),
            source: e,
        })?;

    Error::check_command("zfs clone", output)?;
    Ok(())
}

/// Set the volume size of a zvol.
///
/// Used to grow a zvol after cloning from a smaller image. Only growing
/// is supported — ZFS will error if the new size is smaller.
pub fn set_volsize(zvol: &str, size_gib: u32) -> Result<()> {
    let size_arg = format!("volsize={size_gib}G");

    let output = Command::new("zfs")
        .args(["set", &size_arg, zvol])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs set".to_string(),
            source: e,
        })?;

    Error::check_command("zfs set volsize", output)?;
    Ok(())
}

/// Return the `/dev/zvol/...` block device path for a zvol.
///
/// The kernel creates this device node automatically when the zvol exists.
pub fn device_path(zvol: &str) -> PathBuf {
    PathBuf::from(format!("/dev/zvol/{zvol}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_path_construction() {
        assert_eq!(
            device_path("tank/images/ubuntu-22.04"),
            PathBuf::from("/dev/zvol/tank/images/ubuntu-22.04")
        );
        assert_eq!(
            device_path("tank/vms/myvm"),
            PathBuf::from("/dev/zvol/tank/vms/myvm")
        );
    }
}
