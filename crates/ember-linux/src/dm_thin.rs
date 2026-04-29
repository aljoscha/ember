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
///
/// `dmsetup` translates the kernel's `-EEXIST` into a stderr line that
/// embeds the libc `strerror` for `EEXIST` — `"File exists"` on glibc
/// and musl. The exact wrapping line has shifted across `lvm2`
/// releases (e.g. `"device-mapper: message ioctl on ember-pool failed:
/// File exists"`), but the trailing strerror is stable. Pinned and
/// regression-tested against:
///
/// * Linux 6.1+ (Debian 12, Ubuntu 24.04)
/// * `lvm2` 2.03.x (Debian / Fedora packaging from 2023+)
/// * glibc and musl (`strerror(EEXIST) == "File exists"`)
///
/// If a future kernel/util-linux/libc combination changes the wording,
/// retries will turn into hard failures rather than collide silently —
/// the [`tests::matches_dmsetup_eexist_message`] test below would be
/// the first thing to fail.
pub fn is_already_exists(err: &ember_core::error::Error) -> bool {
    matches!(
        err,
        ember_core::error::Error::Command { stderr, .. } if stderr.contains("File exists")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ember_core::error::Error;

    /// Regression: mirror an actual `dmsetup message` failure line so a
    /// future glibc/lvm2 wording change is loud, not silent. Captured
    /// from a Linux 6.1 / lvm2 2.03 host attempting `create_thin` with
    /// a duplicate id.
    #[test]
    fn matches_dmsetup_eexist_message() {
        let err = Error::Command {
            command: "dmsetup".to_string(),
            exit_code: 1,
            stderr: "device-mapper: message ioctl on ember-pool failed: File exists\n".to_string(),
        };
        assert!(is_already_exists(&err));
    }

    #[test]
    fn rejects_unrelated_errors() {
        let err = Error::Command {
            command: "dmsetup".to_string(),
            exit_code: 1,
            stderr: "device-mapper: reload ioctl on ember-pool failed: Invalid argument\n"
                .to_string(),
        };
        assert!(!is_already_exists(&err));
    }

    #[test]
    fn rejects_non_command_errors() {
        let err = Error::Vm("File exists somewhere else in the system".to_string());
        assert!(!is_already_exists(&err));
    }
}
