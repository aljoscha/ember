//! ZFS dataset operations via the `zfs` CLI.
//!
//! Datasets are filesystem-type containers (not zvols) used as parent
//! namespaces — e.g. `<pool>/images` and `<pool>/vms`.

use std::process::Command;

use crate::error::{Error, Result};

/// Summary information about a ZFS dataset.
#[derive(Debug, Clone)]
pub struct DatasetInfo {
    pub name: String,
    /// Used bytes.
    pub used: u64,
    /// Available bytes.
    pub available: u64,
    /// Mount point, or "none" if not mounted.
    pub mountpoint: String,
}

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

/// Get information about a ZFS dataset.
///
/// Returns an error if the dataset does not exist.
pub fn info(dataset: &str) -> Result<DatasetInfo> {
    let output = Command::new("zfs")
        .args(["list", "-Hp", "-o", "name,used,avail,mountpoint", dataset])
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

    Ok(DatasetInfo {
        name: fields[0].to_string(),
        used: parse_bytes(fields[1], "used")?,
        available: parse_bytes(fields[2], "available")?,
        mountpoint: fields[3].to_string(),
    })
}

/// List child datasets under a parent dataset.
///
/// Returns an empty list if the parent has no children (the parent itself
/// is excluded from the results).
pub fn list(parent: &str) -> Result<Vec<DatasetInfo>> {
    let output = Command::new("zfs")
        .args([
            "list", "-Hp", "-r", "-t", "filesystem",
            "-o", "name,used,avail,mountpoint",
            parent,
        ])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs list".to_string(),
            source: e,
        })?;

    let output = Error::check_command("zfs list", output)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut datasets = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 4 {
            continue;
        }

        // Skip the parent itself — only return children.
        if fields[0] == parent {
            continue;
        }

        let parse_bytes = |s: &str, field: &str| -> Result<u64> {
            s.trim()
                .parse::<u64>()
                .map_err(|_| Error::Zfs(format!("cannot parse {field} value: {s}")))
        };

        datasets.push(DatasetInfo {
            name: fields[0].to_string(),
            used: parse_bytes(fields[1], "used")?,
            available: parse_bytes(fields[2], "available")?,
            mountpoint: fields[3].to_string(),
        });
    }

    Ok(datasets)
}

/// Create a ZFS dataset (filesystem type).
///
/// Sets `mountpoint=none` since crackling uses datasets only as parent
/// namespaces for zvols, not as mounted filesystems.
///
/// Creates parent datasets as needed (`-p` flag).
pub fn create(dataset: &str) -> Result<()> {
    let output = Command::new("zfs")
        .args([
            "create", "-p",
            "-o", "mountpoint=none",
            dataset,
        ])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs create".to_string(),
            source: e,
        })?;

    Error::check_command("zfs create", output)?;
    Ok(())
}

/// Destroy a ZFS dataset.
///
/// With `recursive: true`, also destroys all child datasets and snapshots
/// (`-r` flag). Returns an error if the dataset does not exist.
pub fn destroy(dataset: &str, recursive: bool) -> Result<()> {
    let mut args = vec!["destroy"];
    if recursive {
        args.push("-r");
    }
    args.push(dataset);

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

#[cfg(test)]
mod tests {
    #[test]
    fn parse_dataset_info_line() {
        // Simulate the output of `zfs list -Hp -o name,used,avail,mountpoint`
        let line = "tank/images\t1024\t999999488\tnone";
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0], "tank/images");
        assert_eq!(fields[1].parse::<u64>().unwrap(), 1024);
        assert_eq!(fields[2].parse::<u64>().unwrap(), 999999488);
        assert_eq!(fields[3], "none");
    }

    #[test]
    fn parse_list_output_excludes_parent() {
        // Simulate multi-line output where first line is the parent.
        let output = "tank\t1024\t999999488\tnone\ntank/images\t512\t999999488\tnone\ntank/vms\t512\t999999488\tnone\n";
        let parent = "tank";

        let datasets: Vec<&str> = output
            .lines()
            .filter(|l| !l.is_empty())
            .filter(|l| {
                let name = l.split('\t').next().unwrap_or("");
                name != parent
            })
            .collect();

        assert_eq!(datasets.len(), 2);
        assert!(datasets[0].starts_with("tank/images"));
        assert!(datasets[1].starts_with("tank/vms"));
    }
}
