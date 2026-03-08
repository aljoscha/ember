//! macOS image helpers: ext4 creation with hdiutil mounting.
//!
//! Creates an ext4 filesystem image file from a directory of unpacked
//! OCI layers. Same workflow as Linux but uses `hdiutil attach/detach`
//! instead of `mount -o loop`/`umount`.
//!
//! Requires Homebrew `e2fsprogs` for `mkfs.ext4`.

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// Create an ext4 filesystem image from a rootfs directory.
///
/// Creates a sparse file at `image_path`, formats it with ext4 (from
/// Homebrew e2fsprogs), mounts it via `hdiutil attach`, copies the
/// rootfs content in, then detaches.
///
/// `size_mib` is the total image size in MiB. Use [`estimate_size_mib`]
/// to calculate a suitable size from the rootfs content.
pub fn create(rootfs_dir: &Path, image_path: &Path, size_mib: u64) -> Result<()> {
    // Create sparse file.
    create_sparse_file(image_path, size_mib)?;

    // Format with ext4 (requires Homebrew e2fsprogs).
    mkfs_ext4(image_path)?;

    // Mount via hdiutil, copy rootfs, detach. Always detach even if copy fails.
    let mount_point = hdiutil_attach(image_path)?;

    let copy_result = copy_rootfs(rootfs_dir, &mount_point);
    let detach_result = hdiutil_detach(&mount_point);

    copy_result?;
    detach_result?;

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

/// Mount a raw disk image via `hdiutil attach`. Returns the mount point path.
///
/// Uses `-nomount` is NOT used — we want hdiutil to mount the ext4 filesystem.
/// The `-nobrowse` flag prevents the volume from appearing in Finder.
fn hdiutil_attach(image_path: &Path) -> Result<std::path::PathBuf> {
    let output = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-plist"])
        .arg(image_path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "hdiutil attach".to_string(),
            source: e,
        })?;
    let output = Error::check_command("hdiutil attach", output)?;

    // Parse the plist output to find the mount point.
    // hdiutil -plist outputs XML; the mount point is in system-entities[].mount-point.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mount_point = parse_hdiutil_mount_point(&stdout).ok_or_else(|| {
        Error::Image(format!(
            "hdiutil attach succeeded but could not find mount point in output:\n{stdout}"
        ))
    })?;

    Ok(std::path::PathBuf::from(mount_point))
}

/// Detach (unmount) a disk image previously mounted with `hdiutil attach`.
pub(crate) fn hdiutil_detach(mount_point: &Path) -> Result<()> {
    let output = Command::new("hdiutil")
        .args(["detach"])
        .arg(mount_point)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "hdiutil detach".to_string(),
            source: e,
        })?;
    Error::check_command("hdiutil detach", output)?;
    Ok(())
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

/// Format a file as ext4 using Homebrew e2fsprogs.
///
/// `-F` forces creation on a regular file (not a block device).
/// `-q` suppresses superfluous output.
fn mkfs_ext4(path: &Path) -> Result<()> {
    let output = Command::new("mkfs.ext4")
        .args(["-F", "-q"])
        .arg(path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "mkfs.ext4".to_string(),
            source: e,
        })?;
    Error::check_command("mkfs.ext4", output)?;
    Ok(())
}

/// Copy rootfs contents into the mounted ext4 filesystem.
///
/// Uses `cp -a` to preserve permissions, ownership, symlinks, and timestamps.
fn copy_rootfs(rootfs_dir: &Path, mount_dir: &Path) -> Result<()> {
    let output = Command::new("cp")
        .arg("-a")
        .arg(rootfs_dir.join("."))
        .arg(mount_dir)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "cp -a".to_string(),
            source: e,
        })?;
    Error::check_command("cp -a", output)?;
    Ok(())
}

/// Parse the mount point from `hdiutil attach -plist` XML output.
///
/// Looks for `<key>mount-point</key><string>...</string>` in the plist.
/// This is a simple string search rather than full XML parsing to avoid
/// pulling in an XML dependency.
fn parse_hdiutil_mount_point(plist: &str) -> Option<String> {
    let mut lines = plist.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "<key>mount-point</key>" {
            if let Some(value_line) = lines.next() {
                let trimmed = value_line.trim();
                if let Some(inner) = trimmed.strip_prefix("<string>") {
                    if let Some(path) = inner.strip_suffix("</string>") {
                        return Some(path.to_string());
                    }
                }
            }
        }
    }
    None
}
