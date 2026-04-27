//! Linux device-mapper thin provisioning backend.
//!
//! Thin pools provide block-level copy-on-write storage. A single
//! [`pool::POOL_NAME`] pool aggregates two backing devices (metadata and
//! data) and exposes any number of independent thin volumes addressed by
//! 64-bit numeric IDs. Snapshots and clones are the same primitive
//! ([`thin::create_snap`]) — snapshotting a thin volume produces another
//! thin volume that shares blocks until divergence.
//!
//! See `docs/DM-THIN-SPEC.md` for the full design.

pub mod loop_device;
pub mod pool;
pub mod thin;
pub mod tools;

/// Sectors are always 512 bytes on Linux block devices.
pub const SECTOR_SIZE: u64 = 512;

/// Convert bytes to sectors, rounding up.
pub fn bytes_to_sectors(bytes: u64) -> u64 {
    bytes.div_ceil(SECTOR_SIZE)
}

/// Whether an [`Error`](ember_core::error::Error) reports a kernel `EEXIST`
/// from a `dmsetup message` operation. Used by the `create_thin` /
/// `create_snap` retry loops to detect thin id collisions.
pub fn is_already_exists(err: &ember_core::error::Error) -> bool {
    matches!(
        err,
        ember_core::error::Error::Command { stderr, .. } if stderr.contains("File exists")
    )
}
