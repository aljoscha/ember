//! ZFS pool operations via the `zpool` CLI.

use std::process::Command;

use ember_core::error::{Error, Result};

/// Health status of a ZFS pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolHealth {
    Online,
    Degraded,
    Faulted,
    Offline,
    Removed,
    Unavail,
    /// A health string we don't recognize.
    Unknown(String),
}

impl std::fmt::Display for PoolHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Online => write!(f, "ONLINE"),
            Self::Degraded => write!(f, "DEGRADED"),
            Self::Faulted => write!(f, "FAULTED"),
            Self::Offline => write!(f, "OFFLINE"),
            Self::Removed => write!(f, "REMOVED"),
            Self::Unavail => write!(f, "UNAVAIL"),
            Self::Unknown(s) => write!(f, "{s}"),
        }
    }
}

impl From<&str> for PoolHealth {
    fn from(s: &str) -> Self {
        match s.trim() {
            "ONLINE" => Self::Online,
            "DEGRADED" => Self::Degraded,
            "FAULTED" => Self::Faulted,
            "OFFLINE" => Self::Offline,
            "REMOVED" => Self::Removed,
            "UNAVAIL" => Self::Unavail,
            other => Self::Unknown(other.to_string()),
        }
    }
}

/// Summary information about a ZFS pool.
#[derive(Debug, Clone)]
pub struct PoolInfo {
    pub name: String,
    pub health: PoolHealth,
    /// Total size in bytes.
    pub size: u64,
    /// Allocated bytes.
    pub allocated: u64,
    /// Free bytes.
    pub free: u64,
}

/// Check whether a ZFS pool exists.
pub fn exists(pool: &str) -> Result<bool> {
    let output = Command::new("zpool")
        .args(["list", "-H", "-o", "name", pool])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zpool list".to_string(),
            source: e,
        })?;

    // `zpool list <pool>` exits 0 if found, non-zero if not.
    Ok(output.status.success())
}

/// Get information about a ZFS pool.
///
/// Returns an error if the pool does not exist.
pub fn status(pool: &str) -> Result<PoolInfo> {
    let output = Command::new("zpool")
        .args(["list", "-Hp", "-o", "name,health,size,alloc,free", pool])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zpool list".to_string(),
            source: e,
        })?;

    let output = Error::check_command("zpool list", output)?;

    let line = String::from_utf8_lossy(&output.stdout);
    let line = line.trim();
    let fields: Vec<&str> = line.split('\t').collect();

    if fields.len() < 5 {
        return Err(Error::Zfs(format!("unexpected zpool list output: {line}")));
    }

    Ok(PoolInfo {
        name: fields[0].to_string(),
        health: PoolHealth::from(fields[1]),
        size: super::parse_u64(fields[2], "size")?,
        allocated: super::parse_u64(fields[3], "allocated")?,
        free: super::parse_u64(fields[4], "free")?,
    })
}

/// Create a new ZFS pool on the given device.
///
/// Uses `zpool create -f` with `ashift=12` (4K sectors) and `mountpoint=none`
/// since ember uses zvols, not mounted filesystems.
pub fn create(pool: &str, device: &str) -> Result<()> {
    let output = Command::new("zpool")
        .args([
            "create",
            "-f",
            "-o",
            "ashift=12",
            "-O",
            "mountpoint=none",
            pool,
            device,
        ])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zpool create".to_string(),
            source: e,
        })?;

    Error::check_command("zpool create", output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_health_from_str() {
        assert_eq!(PoolHealth::from("ONLINE"), PoolHealth::Online);
        assert_eq!(PoolHealth::from("DEGRADED"), PoolHealth::Degraded);
        assert_eq!(PoolHealth::from("FAULTED"), PoolHealth::Faulted);
        assert_eq!(PoolHealth::from("  ONLINE  "), PoolHealth::Online);
        assert_eq!(
            PoolHealth::from("SOMETHING"),
            PoolHealth::Unknown("SOMETHING".to_string())
        );
    }

    #[test]
    fn pool_health_display() {
        assert_eq!(PoolHealth::Online.to_string(), "ONLINE");
        assert_eq!(PoolHealth::Degraded.to_string(), "DEGRADED");
        assert_eq!(PoolHealth::Unknown("FOO".to_string()).to_string(), "FOO");
    }
}
