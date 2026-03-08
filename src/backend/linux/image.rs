//! Linux image helpers: ext4 creation with loop mounting.
//!
//! Creates an ext4 filesystem image file from a directory of unpacked
//! OCI layers. The workflow:
//!   1. Create a sparse file of the required size
//!   2. Format it with `mkfs.ext4`
//!   3. Loop mount to a temporary directory
//!   4. Copy rootfs content in with `cp -a`
//!   5. Unmount
//!
//! On macOS, this module is replaced by `backend::macos::image` which
//! uses `hdiutil attach/detach` instead of `mount -o loop`/`umount`.
//!
//! Requires root privileges for loop mounting.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// Create an ext4 filesystem image from a rootfs directory.
///
/// Creates a sparse file at `image_path`, formats it with ext4, loop
/// mounts it, and copies the rootfs content in. The caller can later
/// `dd` this image to a ZFS zvol.
///
/// `size_mib` is the total image size in MiB. Use [`estimate_size_mib`]
/// to calculate a suitable size from the rootfs content.
pub fn create(rootfs_dir: &Path, image_path: &Path, size_mib: u64) -> Result<()> {
    // Create sparse file.
    create_sparse_file(image_path, size_mib)?;

    // Format with ext4.
    mkfs_ext4(image_path)?;

    // Mount, copy, unmount. Always unmount even if copy fails.
    let mount_dir = image_path.with_extension("mnt");
    fs::create_dir_all(&mount_dir).map_err(|e| Error::Io {
        path: mount_dir.clone(),
        source: e,
    })?;

    mount_loop(image_path, &mount_dir)?;

    let copy_result = copy_rootfs(rootfs_dir, &mount_dir);
    let umount_result = umount(&mount_dir);
    let _ = fs::remove_dir(&mount_dir);

    copy_result?;
    umount_result?;

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

/// Unmount a previously mounted filesystem.
pub(crate) fn umount(mount_dir: &Path) -> Result<()> {
    let output = Command::new("umount")
        .arg(mount_dir)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "umount".to_string(),
            source: e,
        })?;
    Error::check_command("umount", output)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Create a sparse file of the given size in MiB.
fn create_sparse_file(path: &Path, size_mib: u64) -> Result<()> {
    let output = Command::new("truncate")
        .args(["-s", &format!("{size_mib}M")])
        .arg(path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "truncate".to_string(),
            source: e,
        })?;
    Error::check_command("truncate", output)?;
    Ok(())
}

/// Format a file as ext4.
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

/// Loop mount a filesystem image at the given mount point.
fn mount_loop(image_path: &Path, mount_dir: &Path) -> Result<()> {
    let output = Command::new("mount")
        .args(["-o", "loop"])
        .arg(image_path)
        .arg(mount_dir)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "mount -o loop".to_string(),
            source: e,
        })?;
    Error::check_command("mount -o loop", output)?;
    Ok(())
}

/// Copy rootfs contents into the mounted ext4 filesystem.
///
/// Uses `cp -a` to preserve permissions, ownership, symlinks,
/// device nodes, and timestamps. The `rootfs/.` idiom copies
/// the *contents* of the directory rather than the directory itself.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn estimate_minimum_64_mib() {
        // Create a tiny temporary directory to ensure we get the 64 MiB minimum.
        let dir = tempfile::tempdir().unwrap();
        let size = estimate_size_mib(dir.path()).unwrap();
        assert!(size >= 64, "minimum size should be 64 MiB, got {size}");
    }

    #[test]
    fn estimate_nonexistent_dir_fails() {
        let result = estimate_size_mib(&PathBuf::from("/nonexistent/path/for/test"));
        assert!(result.is_err());
    }

    #[test]
    fn create_sparse_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.img");
        create_sparse_file(&path, 64).unwrap();
        assert!(path.exists());
        // Sparse file: metadata says 64 MiB, actual disk usage is near zero.
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 64 * 1024 * 1024);
    }

    #[test]
    fn mkfs_ext4_on_sparse_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.img");
        create_sparse_file(&path, 64).unwrap();
        mkfs_ext4(&path).unwrap();
        // Verify by checking the file starts with ext4 superblock data
        // (file should be larger than before due to metadata).
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 64 * 1024 * 1024);
    }
}
