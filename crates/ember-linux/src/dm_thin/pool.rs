//! `dmsetup` wrappers for the `thin-pool` target.
//!
//! A thin pool is the kernel-side container holding metadata + data
//! devices and exposing thin volumes as snapshot-capable block devices.
//! Ember runs a single named pool ([`POOL_NAME`]) per installation.

use std::path::{Path, PathBuf};
use std::process::Command;

use ember_core::error::{Error, Result};

/// Device-mapper name of the singleton thin pool used by ember.
pub const POOL_NAME: &str = "ember-pool";

/// Default pool block size in 512-byte sectors (= 64 KiB).
///
/// Permanent at pool creation. Smaller blocks improve sharing across
/// snapshots but inflate metadata; larger blocks reduce metadata at the
/// cost of write amplification when only part of a block is dirtied.
pub const DEFAULT_BLOCK_SIZE_SECTORS: u32 = 128;

/// Default low-water-mark in pool blocks. With the default 64 KiB block
/// size this is 2 GiB of free space — the threshold at which the kernel
/// raises a `dmeventd` notification.
pub const DEFAULT_LOW_WATER_BLOCKS: u64 = 32_768;

/// Operating mode reported by `dmsetup status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolMode {
    /// Normal operation.
    ReadWrite,
    /// Pool entered read-only after a metadata error or admin request.
    ReadOnly,
    /// Pool ran out of data blocks. New writes return EIO until grown.
    OutOfDataSpace,
    /// Pool is unrecoverable; metadata or device-level failure.
    Failed,
}

/// Status snapshot returned by [`status`].
///
/// Sizes are in pool blocks (not sectors): each block is
/// [`DEFAULT_BLOCK_SIZE_SECTORS`] × 512 bytes by default.
#[derive(Debug)]
pub struct PoolStatus {
    pub used_metadata_blocks: u64,
    pub total_metadata_blocks: u64,
    pub used_data_blocks: u64,
    pub total_data_blocks: u64,
    pub mode: PoolMode,
}

/// List active device-mapper device names whose name starts with
/// `prefix`. Useful for finding all `ember-vm-*` and `ember-img-*`
/// volumes during teardown.
pub fn list_with_prefix(prefix: &str) -> Result<Vec<String>> {
    let output = Command::new("dmsetup")
        .arg("ls")
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup ls".to_string(),
            source: e,
        })?;
    let output = Error::check_command("dmsetup ls", output)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            if name.starts_with(prefix) {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect())
}

/// Whether a device-mapper device with the given name is currently active.
///
/// Uses `dmsetup info` which exits 0 when the device exists, non-zero
/// otherwise.
pub fn exists(name: &str) -> Result<bool> {
    let output = Command::new("dmsetup")
        .args(["info", "--noheadings", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup info".to_string(),
            source: e,
        })?;
    Ok(output.status.success())
}

/// Build a `thin-pool` table line.
///
/// The format is documented in
/// `Documentation/admin-guide/device-mapper/thin-provisioning.rst`:
/// `0 <data_sectors> thin-pool <metadata_dev> <data_dev> <block_size> <low_water>`.
fn pool_table(
    metadata_dev: &Path,
    data_dev: &Path,
    data_sectors: u64,
    block_size_sectors: u32,
    low_water_blocks: u64,
) -> String {
    format!(
        "0 {data_sectors} thin-pool {} {} {block_size_sectors} {low_water_blocks}",
        metadata_dev.display(),
        data_dev.display(),
    )
}

/// Activate a thin pool from existing metadata + data devices.
///
/// If the metadata superblock is all zero the kernel formats a fresh pool;
/// otherwise it imports the existing metadata. Callers wanting a fresh
/// pool must zero the first 4 KiB of the metadata device beforehand.
pub fn create(
    name: &str,
    metadata_dev: &Path,
    data_dev: &Path,
    data_sectors: u64,
    block_size_sectors: u32,
    low_water_blocks: u64,
) -> Result<()> {
    let table = pool_table(
        metadata_dev,
        data_dev,
        data_sectors,
        block_size_sectors,
        low_water_blocks,
    );
    let output = Command::new("dmsetup")
        .args(["create", name, "--table", &table])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup create".to_string(),
            source: e,
        })?;
    Error::check_command("dmsetup create thin-pool", output)?;
    Ok(())
}

/// Tear down the thin pool. Does not destroy the backing devices or
/// metadata — those persist for re-activation later.
///
/// Returns an error if any thin volume is still active. Callers should
/// deactivate all thin volumes before tearing down the pool.
pub fn remove(name: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["remove", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup remove".to_string(),
            source: e,
        })?;
    Error::check_command("dmsetup remove", output)?;
    Ok(())
}

/// Send a control message to the thin pool.
///
/// Most thin-pool operations (`create_thin`, `create_snap`, `delete`,
/// `set_transaction_id`, …) are delivered this way rather than via
/// dedicated dmsetup subcommands.
pub fn message(name: &str, msg: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["message", name, "0", msg])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup message".to_string(),
            source: e,
        })?;
    Error::check_command("dmsetup message", output)?;
    Ok(())
}

/// Reload the pool table with new parameters (typically a larger
/// `data_sectors` after growing the data device).
///
/// Suspend → load → resume sequence is required by the kernel for a
/// live table swap.
pub fn reload(
    name: &str,
    metadata_dev: &Path,
    data_dev: &Path,
    data_sectors: u64,
    block_size_sectors: u32,
    low_water_blocks: u64,
) -> Result<()> {
    let table = pool_table(
        metadata_dev,
        data_dev,
        data_sectors,
        block_size_sectors,
        low_water_blocks,
    );
    suspend(name)?;
    let load = Command::new("dmsetup")
        .args(["load", name, "--table", &table])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup load".to_string(),
            source: e,
        })?;
    if let Err(e) = Error::check_command("dmsetup load thin-pool", load) {
        // Best-effort resume to leave the pool live before returning.
        let _ = resume(name);
        return Err(e);
    }
    resume(name)
}

/// Path to the activated thin-pool device. Useful for building thin
/// volume tables that reference the pool by `/dev/mapper/...`.
pub fn device_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/dev/mapper/{name}"))
}

/// Suspend a device-mapper device. Required before reloading a table.
pub fn suspend(name: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["suspend", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup suspend".to_string(),
            source: e,
        })?;
    Error::check_command("dmsetup suspend", output)?;
    Ok(())
}

/// Resume a previously suspended device-mapper device.
pub fn resume(name: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["resume", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup resume".to_string(),
            source: e,
        })?;
    Error::check_command("dmsetup resume", output)?;
    Ok(())
}

/// Query thin-pool status via `dmsetup status`.
///
/// Output format documented in
/// `Documentation/admin-guide/device-mapper/thin-provisioning.rst`:
///
/// ```text
/// <start> <length> thin-pool <txn_id> <used_meta>/<total_meta>
///   <used_data>/<total_data> <held_meta_root>
///   <ro|rw|out_of_data_space|failed>
///   <discard_passdown|no_discard_passdown>
///   <error_if_no_space|queue_if_no_space>
///   <needs_check|-> <metadata_low_watermark>
/// ```
pub fn status(name: &str) -> Result<PoolStatus> {
    let output = Command::new("dmsetup")
        .args(["status", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup status".to_string(),
            source: e,
        })?;
    let output = Error::check_command("dmsetup status", output)?;
    parse_status(&String::from_utf8_lossy(&output.stdout))
}

fn parse_status(line: &str) -> Result<PoolStatus> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    // Minimum: start, length, "thin-pool", txn_id, meta, data, held_meta, mode → 8.
    if fields.len() < 8 || fields[2] != "thin-pool" {
        return Err(Error::Command {
            command: "dmsetup status thin-pool".to_string(),
            exit_code: 0,
            stderr: format!("unexpected status format: {line}"),
        });
    }
    let (used_meta, total_meta) = parse_fraction(fields[4])?;
    let (used_data, total_data) = parse_fraction(fields[5])?;
    let mode = match fields[7] {
        "rw" => PoolMode::ReadWrite,
        "ro" => PoolMode::ReadOnly,
        "out_of_data_space" => PoolMode::OutOfDataSpace,
        // The kernel sometimes reports "Fail" or omits trailing fields when
        // the pool is unrecoverable.
        "Fail" | "failed" => PoolMode::Failed,
        other => {
            return Err(Error::Command {
                command: "dmsetup status thin-pool".to_string(),
                exit_code: 0,
                stderr: format!("unknown pool mode: {other}"),
            });
        }
    };
    Ok(PoolStatus {
        used_metadata_blocks: used_meta,
        total_metadata_blocks: total_meta,
        used_data_blocks: used_data,
        total_data_blocks: total_data,
        mode,
    })
}

fn parse_fraction(s: &str) -> Result<(u64, u64)> {
    let (used, total) = s.split_once('/').ok_or_else(|| Error::Command {
        command: "dmsetup status thin-pool".to_string(),
        exit_code: 0,
        stderr: format!("expected used/total fraction, got: {s}"),
    })?;
    let used = used.parse::<u64>().map_err(|e| Error::Command {
        command: "dmsetup status thin-pool".to_string(),
        exit_code: 0,
        stderr: format!("invalid used field {used:?}: {e}"),
    })?;
    let total = total.parse::<u64>().map_err(|e| Error::Command {
        command: "dmsetup status thin-pool".to_string(),
        exit_code: 0,
        stderr: format!("invalid total field {total:?}: {e}"),
    })?;
    Ok((used, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_rw() {
        let s = "0 209715200 thin-pool 12 1234/2048 5678/100000 - rw \
                 discard_passdown queue_if_no_space - 1024";
        let st = parse_status(s).unwrap();
        assert_eq!(st.used_metadata_blocks, 1234);
        assert_eq!(st.total_metadata_blocks, 2048);
        assert_eq!(st.used_data_blocks, 5678);
        assert_eq!(st.total_data_blocks, 100_000);
        assert_eq!(st.mode, PoolMode::ReadWrite);
    }

    #[test]
    fn parse_status_out_of_data_space() {
        let s = "0 209715200 thin-pool 7 100/2048 100000/100000 - out_of_data_space \
                 no_discard_passdown error_if_no_space needs_check 1024";
        let st = parse_status(s).unwrap();
        assert_eq!(st.mode, PoolMode::OutOfDataSpace);
        assert_eq!(st.used_data_blocks, st.total_data_blocks);
    }

    #[test]
    fn parse_status_failed() {
        let s = "0 209715200 thin-pool 0 0/0 0/0 - Fail";
        let st = parse_status(s).unwrap();
        assert_eq!(st.mode, PoolMode::Failed);
    }

    #[test]
    fn parse_status_rejects_bad_target() {
        let s = "0 100 linear 0 0/0 0/0 - rw";
        assert!(parse_status(s).is_err());
    }

    #[test]
    fn pool_table_format() {
        let t = pool_table(
            Path::new("/dev/loop0"),
            Path::new("/dev/loop1"),
            1_048_576,
            128,
            32_768,
        );
        assert_eq!(t, "0 1048576 thin-pool /dev/loop0 /dev/loop1 128 32768");
    }
}
