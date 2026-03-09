//! macOS image helpers: ext4 creation using `mkfs.ext4 -d`.
//!
//! Creates an ext4 filesystem image file from a directory of unpacked
//! OCI layers. On macOS, the kernel doesn't support ext4, so we can't
//! use `mount -o loop` (Linux) or `hdiutil attach` (macOS, HFS+/APFS only).
//! Instead, we use `mkfs.ext4 -d <rootfs_dir>` which creates and populates
//! the ext4 filesystem in a single step — no mount required.
//!
//! Requires Homebrew `e2fsprogs` for `mkfs.ext4` (`brew install e2fsprogs`).

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// Create an ext4 filesystem image from a rootfs directory.
///
/// Uses `mkfs.ext4 -d <rootfs_dir>` from Homebrew e2fsprogs to create
/// and populate the ext4 image in a single step. This avoids the need
/// to mount the image (macOS doesn't support ext4 mounts natively).
///
/// `size_mib` is the total image size in MiB. Use [`estimate_size_mib`]
/// to calculate a suitable size from the rootfs content.
pub fn create(rootfs_dir: &Path, image_path: &Path, size_mib: u64) -> Result<()> {
    // Create sparse file.
    create_sparse_file(image_path, size_mib)?;

    // Format with ext4 and populate from rootfs in one step.
    // The -d flag copies the contents of rootfs_dir into the new filesystem,
    // preserving permissions, ownership, and symlinks.
    mkfs_ext4_from_dir(image_path, rootfs_dir)?;

    Ok(())
}

/// Estimate the ext4 image size needed to hold `rootfs_dir` contents.
///
/// Returns size in MiB with overhead for ext4 metadata and free space.
/// Minimum returned size is 64 MiB.
pub fn estimate_size_mib(rootfs_dir: &Path) -> Result<u64> {
    let output = Command::new("du")
        .args(["-sm"])
        .arg(rootfs_dir)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "du -sm".to_string(),
            source: e,
        })?;
    let output = Error::check_command("du -sm", output)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let size_str = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| Error::Image("failed to parse du output".to_string()))?;
    let size_mib: u64 = size_str
        .parse()
        .map_err(|_| Error::Image(format!("failed to parse rootfs size from du: {size_str}")))?;

    // 50% overhead for ext4 metadata + breathing room, minimum 64 MiB.
    Ok((size_mib * 3 / 2).max(64))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Create a sparse file of the given size in MiB.
fn create_sparse_file(path: &Path, size_mib: u64) -> Result<()> {
    let output = Command::new("truncate")
        .args(["-s", &format!("{}m", size_mib)])
        .arg(path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "truncate".to_string(),
            source: e,
        })?;
    Error::check_command("truncate", output)?;
    Ok(())
}

/// Create an ext4 filesystem from a directory using `mkfs.ext4 -d`.
///
/// The `-d` flag populates the new filesystem with the contents of
/// `rootfs_dir`, preserving permissions, ownership, and symlinks.
/// This is equivalent to mkfs + mount + cp -a + umount but doesn't
/// require mounting (critical on macOS where ext4 mounts aren't supported).
///
/// Uses [`super::storage::find_e2fsprogs_tool`] to locate `mkfs.ext4`
/// in Homebrew's keg-only installation path.
fn mkfs_ext4_from_dir(image_path: &Path, rootfs_dir: &Path) -> Result<()> {
    let mkfs = super::storage::find_e2fsprogs_tool("mkfs.ext4");
    let output = Command::new(&mkfs)
        .args(["-F", "-q"])
        .arg("-d")
        .arg(rootfs_dir)
        .arg(image_path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "mkfs.ext4".to_string(),
            source: e,
        })?;
    Error::check_command("mkfs.ext4", output)?;
    Ok(())
}
