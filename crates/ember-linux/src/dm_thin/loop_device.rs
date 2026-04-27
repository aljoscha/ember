//! `losetup` wrappers for attaching backing files as loop block devices.
//!
//! The dm-thin backend uses loop devices to expose sparse `metadata.img` and
//! `data.img` files as block devices that the kernel can assemble into a
//! thin pool. Attachment is per-`ember` invocation: the loop device must be
//! re-attached after every reboot (state is in-memory).

use std::path::{Path, PathBuf};
use std::process::Command;

use ember_core::error::{Error, Result};

/// Attach `file` to the next available loop device.
///
/// Returns the loop device path (e.g., `/dev/loop0`).
pub fn attach(file: &Path) -> Result<PathBuf> {
    let output = Command::new("losetup")
        .args(["-f", "--show"])
        .arg(file)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "losetup".to_string(),
            source: e,
        })?;

    let output = Error::check_command("losetup -f --show", output)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout.trim();
    if path.is_empty() {
        return Err(Error::Command {
            command: "losetup -f --show".to_string(),
            exit_code: 0,
            stderr: format!(
                "expected a loop device path on stdout, got empty output for {}",
                file.display()
            ),
        });
    }
    Ok(PathBuf::from(path))
}

/// Detach a loop device.
///
/// Idempotent in spirit but not in fact: callers should ignore failures
/// during teardown if the loop device may already be gone.
pub fn detach(loop_dev: &Path) -> Result<()> {
    let output = Command::new("losetup")
        .arg("-d")
        .arg(loop_dev)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "losetup -d".to_string(),
            source: e,
        })?;
    Error::check_command("losetup -d", output)?;
    Ok(())
}

/// Re-read the backing file's size into the loop device.
///
/// Required after `truncate`-ing the data backing file when growing the
/// pool: the loop driver caches the size, so the kernel doesn't see the
/// new bytes until we ask it to refresh.
pub fn refresh_size(loop_dev: &Path) -> Result<()> {
    let output = Command::new("losetup")
        .arg("-c")
        .arg(loop_dev)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "losetup -c".to_string(),
            source: e,
        })?;
    Error::check_command("losetup -c", output)?;
    Ok(())
}

/// Look up the loop device currently backing `file`, if any.
pub fn find_for(file: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("losetup")
        .args(["-j", "-O", "NAME", "--noheadings"])
        .arg(file)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "losetup -j".to_string(),
            source: e,
        })?;

    // `losetup -j` exits 0 even when the file has no loop attached.
    let output = Error::check_command("losetup -j", output)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next().map(str::trim).filter(|s| !s.is_empty());
    Ok(first.map(PathBuf::from))
}
