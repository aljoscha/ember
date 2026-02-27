//! Image-to-zvol pipeline: write ext4 image to zvol and create @base snapshot.
//!
//! This is the final stage of the image pull workflow described in SPEC.md:
//!
//! ```text
//! ext4 image file
//!     │  (dd to zvol)
//!     ▼
//! ZFS zvol: <pool>/images/<name>-<tag>
//!     │  (zfs snapshot)
//!     ▼
//! ZFS snapshot: <pool>/images/<name>-<tag>@base
//! ```
//!
//! After this step, VM creation can instantly clone the @base snapshot.

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::zfs;

/// Write an ext4 image file to a ZFS zvol and create a `@base` snapshot.
///
/// The zvol must already exist with sufficient size (at least as large as
/// the ext4 image). After this call the zvol contains the ext4 filesystem
/// and has a `@base` snapshot ready for per-VM cloning.
///
/// # Arguments
///
/// * `image_path` — Path to the ext4 image file (created by [`crate::image::ext4::create`]).
/// * `zvol` — Full ZFS zvol name (e.g. `tank/ember/images/library-alpine-latest`).
pub fn write_to_zvol(image_path: &Path, zvol: &str) -> Result<()> {
    let dev_path = zfs::volume::device_path(zvol);

    // Wait for the zvol device node to appear. ZFS creates /dev/zvol/...
    // asynchronously via udev, so it may not be ready immediately after
    // `zfs create -V`.
    wait_for_device(&dev_path)?;

    // dd the ext4 image to the zvol block device.
    dd_image(image_path, &dev_path)?;

    // Create the @base snapshot for cloning.
    zfs::snapshot::create(zvol, "base")?;

    Ok(())
}

/// Wait for a block device node to appear, with timeout.
///
/// Runs `udevadm settle` first to flush pending udev events, then polls
/// for up to 5 seconds.
pub fn wait_for_device(dev_path: &Path) -> Result<()> {
    // Ask udev to process pending events.
    let _ = Command::new("udevadm")
        .args(["settle", "--timeout=10"])
        .output();

    // Poll for device existence (50 × 100ms = 5s timeout).
    for _ in 0..50 {
        if dev_path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    Err(Error::Zfs(format!(
        "timed out waiting for zvol device node at {}",
        dev_path.display()
    )))
}

/// Copy an image file to a block device with `dd`.
///
/// Uses a 4 MiB block size for throughput and `conv=fsync` to ensure
/// data is flushed to disk before returning.
fn dd_image(src: &Path, dst: &Path) -> Result<()> {
    let if_arg = format!("if={}", src.display());
    let of_arg = format!("of={}", dst.display());

    let output = Command::new("dd")
        .args([&if_arg, &of_arg, "bs=4M", "conv=fsync", "status=none"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dd".to_string(),
            source: e,
        })?;

    Error::check_command("dd", output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn wait_for_existing_file_succeeds() {
        // wait_for_device should return immediately for a path that exists.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-device");
        std::fs::write(&path, b"").unwrap();

        wait_for_device(&path).unwrap();
    }

    #[test]
    fn wait_for_nonexistent_times_out() {
        // Reduce the timeout impact by checking a clearly nonexistent path.
        // This test is slow (~5s) so it's ignored by default.
        // Uncomment #[ignore] removal to run manually.
        let path = PathBuf::from("/dev/zvol/nonexistent-test-pool/should-not-exist");
        if path.exists() {
            // Skip if this somehow exists on the system.
            return;
        }
        let result = wait_for_device(&path);
        assert!(result.is_err());
    }
}
