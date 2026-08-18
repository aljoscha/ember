//! `dmsetup` wrappers for the `thin-pool` target.
//!
//! A thin pool is the kernel-side container holding metadata + data
//! devices and exposing thin volumes as snapshot-capable block devices.
//! Ember runs one named pool per installation; the pool name is
//! derived from the install's namespace by [`name`] so two installs
//! on the same host don't share a pool.

use std::path::{Path, PathBuf};

use ember_core::error::{Error, Result};

use crate::dm;

/// dm-thin pool name for an installation.
///
/// `instance_id` is `Some(ns)` for a per-installation pool
/// (`ember-{ns}-pool`) and `None` for legacy configs that predate
/// per-installation isolation. Older binaries created the singleton
/// `ember-pool`, and that exact name must remain reachable across
/// upgrades — any other string in legacy mode would point at a
/// non-existent pool (or, worse, a `ember--pool` typo that init
/// would race to create), orphaning the data on disk.
pub fn name(instance_id: Option<&str>) -> String {
    match instance_id {
        None => "ember-pool".to_string(),
        Some(id) => format!("ember-{id}-pool"),
    }
}

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

/// Size of one metadata block, in bytes. Fixed by the kernel and
/// independent of the pool's data block size. Needed to turn the
/// metadata block counts in [`PoolStatus`] into bytes.
pub const METADATA_BLOCK_SIZE: u64 = 4096;

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

/// Ensure the kernel has the `thin-pool` device-mapper target available.
pub fn ensure_target_loaded() -> Result<()> {
    dm::ensure_target("dm-thin-pool", "thin-pool", "CONFIG_DM_THIN_PROVISIONING")
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
    dm::create(
        name,
        &pool_table(
            metadata_dev,
            data_dev,
            data_sectors,
            block_size_sectors,
            low_water_blocks,
        ),
    )
}

/// Send a control message to the thin pool.
///
/// Most thin-pool operations (`create_thin`, `create_snap`, `delete`,
/// `set_transaction_id`, …) are delivered this way rather than via
/// dedicated dmsetup subcommands.
pub fn message(name: &str, msg: &str) -> Result<()> {
    dm::message(name, msg)
}

/// A reserved pool metadata snapshot, released when dropped.
///
/// Reading thin-volume mappings needs a snapshot, because the live
/// metadata device belongs to the kernel and cannot be walked
/// underneath it. A pool holds at most one snapshot at a time, which is
/// why this is a guard rather than a pair of calls: the release has to
/// happen on every path out, including an early `?` return and a panic.
/// A leaked reservation blocks the next reader and pins metadata blocks
/// that the pool would otherwise be free to reuse.
pub struct MetadataSnap {
    pool: String,
}

impl MetadataSnap {
    /// Reserve the pool's metadata snapshot.
    ///
    /// Fails if one is already held. That is usually a reservation
    /// stranded by a killed process rather than a live reader, so the
    /// error spells out the manual release.
    pub fn reserve(pool_name: &str) -> Result<Self> {
        match message(pool_name, "reserve_metadata_snap") {
            Ok(()) => Ok(Self {
                pool: pool_name.to_string(),
            }),
            Err(e) if dm::is_busy(&e) => Err(Error::Pool(format!(
                "dm-thin pool '{pool_name}' already holds a metadata snapshot. \
                 If nothing else is reading the pool, an earlier run left it behind: \
                 release it with `dmsetup message {pool_name} 0 release_metadata_snap`"
            ))),
            Err(e) => Err(e),
        }
    }
}

impl Drop for MetadataSnap {
    fn drop(&mut self) {
        // We never force-release a snapshot we did not take, and by the
        // same token there is nothing useful to do if releasing our own
        // fails. Swallowing it here at least keeps the failure from
        // masking whatever error is already unwinding.
        let _ = message(&self.pool, "release_metadata_snap");
    }
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
    dm::reload(
        name,
        &pool_table(
            metadata_dev,
            data_dev,
            data_sectors,
            block_size_sectors,
            low_water_blocks,
        ),
    )
}

/// Path to the activated thin-pool device. Useful for building thin
/// volume tables that reference the pool by `/dev/mapper/...`.
pub fn device_path(name: &str) -> PathBuf {
    dm::device_path(name)
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
    parse_status(&dm::status(name)?)
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

    #[test]
    fn name_for_new_install_embeds_namespace() {
        assert_eq!(name(Some("a3f4")), "ember-a3f4-pool");
    }

    /// Locked: legacy hosts have a pool named `ember-pool` in the
    /// kernel and any other string here would orphan their data.
    #[test]
    fn name_for_legacy_install_is_unprefixed() {
        assert_eq!(name(None), "ember-pool");
    }
}
