//! ZFS volume (zvol) operations via the `zfs` CLI.
//!
//! Zvols are block devices backed by ZFS. Ember uses them as root drives
//! for Firecracker VMs — image zvols under `<pool>/images/` and per-VM clones
//! under `<pool>/vms/`.

use std::path::PathBuf;
use std::process::Command;

use crate::error::{Error, Result};

/// Summary information about a ZFS zvol.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub name: String,
    /// Volume size in bytes (the logical size, as set at creation).
    pub volsize: u64,
    /// Bytes used on disk (after compression/CoW).
    pub used: u64,
    /// Bytes referenced by this volume.
    pub referenced: u64,
}

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

/// Get information about a ZFS zvol.
///
/// Returns an error if the zvol does not exist.
pub fn info(zvol: &str) -> Result<VolumeInfo> {
    let output = Command::new("zfs")
        .args([
            "list", "-Hp", "-t", "volume",
            "-o", "name,volsize,used,refer",
            zvol,
        ])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs list".to_string(),
            source: e,
        })?;

    let output = Error::check_command("zfs list", output)?;

    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.trim();
    let fields: Vec<&str> = line.split('\t').collect();

    if fields.len() < 4 {
        return Err(Error::Zfs(format!(
            "unexpected zfs list output: {line}"
        )));
    }

    let parse_bytes = |s: &str, field: &str| -> Result<u64> {
        s.trim()
            .parse::<u64>()
            .map_err(|_| Error::Zfs(format!("cannot parse {field} value: {s}")))
    };

    Ok(VolumeInfo {
        name: fields[0].to_string(),
        volsize: parse_bytes(fields[1], "volsize")?,
        used: parse_bytes(fields[2], "used")?,
        referenced: parse_bytes(fields[3], "referenced")?,
    })
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
    let mut args = vec!["destroy"];
    if recursive {
        args.push("-r");
    }
    args.push(zvol);

    let output = Command::new("zfs")
        .args(&args)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs destroy".to_string(),
            source: e,
        })?;

    Error::check_command("zfs destroy", output)?;
    Ok(())
}

/// Return the `/dev/zvol/...` block device path for a zvol.
///
/// The kernel creates this device node automatically when the zvol exists.
pub fn device_path(zvol: &str) -> PathBuf {
    PathBuf::from(format!("/dev/zvol/{zvol}"))
}

/// List child zvols under a parent dataset.
///
/// Returns an empty list if there are no zvols under the parent.
/// The parent dataset itself is not included (it's a filesystem, not a volume).
pub fn list(parent: &str) -> Result<Vec<VolumeInfo>> {
    let output = Command::new("zfs")
        .args([
            "list", "-Hp", "-r", "-t", "volume",
            "-o", "name,volsize,used,refer",
            parent,
        ])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs list".to_string(),
            source: e,
        })?;

    let output = Error::check_command("zfs list", output)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut volumes = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 4 {
            continue;
        }

        let parse_bytes = |s: &str, field: &str| -> Result<u64> {
            s.trim()
                .parse::<u64>()
                .map_err(|_| Error::Zfs(format!("cannot parse {field} value: {s}")))
        };

        volumes.push(VolumeInfo {
            name: fields[0].to_string(),
            volsize: parse_bytes(fields[1], "volsize")?,
            used: parse_bytes(fields[2], "used")?,
            referenced: parse_bytes(fields[3], "referenced")?,
        });
    }

    Ok(volumes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_volume_info_line() {
        // Simulate `zfs list -Hp -t volume -o name,volsize,used,refer`
        let line = "tank/images/ubuntu-22.04\t4294967296\t1048576\t1048576";
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0], "tank/images/ubuntu-22.04");
        assert_eq!(fields[1].parse::<u64>().unwrap(), 4294967296); // 4 GiB
        assert_eq!(fields[2].parse::<u64>().unwrap(), 1048576); // 1 MiB
        assert_eq!(fields[3].parse::<u64>().unwrap(), 1048576);
    }

    #[test]
    fn parse_list_output_multiple_volumes() {
        let output = "tank/images/ubuntu-22.04\t4294967296\t1048576\t1048576\ntank/images/alpine-3.18\t2147483648\t524288\t524288\n";

        let volumes: Vec<Vec<&str>> = output
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split('\t').collect())
            .collect();

        assert_eq!(volumes.len(), 2);
        assert_eq!(volumes[0][0], "tank/images/ubuntu-22.04");
        assert_eq!(volumes[1][0], "tank/images/alpine-3.18");
    }

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
