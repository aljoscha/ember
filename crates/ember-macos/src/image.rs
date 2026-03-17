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

use ember_core::error::{Error, Result};

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

    // Shrink the filesystem to its minimum size, then truncate the file
    // to match.  This reclaims the generous headroom from estimate_size_mib
    // so the stored image is as small as possible.
    shrink_to_fit(image_path)?;

    Ok(())
}

/// Estimate the ext4 image size needed to hold `rootfs_dir` contents.
///
/// Returns size in MiB with overhead for ext4 metadata and free space.
/// Minimum returned size is 256 MiB.
///
/// On macOS, `du` reports APFS-compressed disk usage which can be far
/// smaller than what ext4 needs (APFS transparently compresses text-heavy
/// content like HTML docs and man pages).  We therefore compute the
/// *apparent* total size — the sum of every file's logical byte count —
/// which is what ext4 will actually store.
pub fn estimate_size_mib(rootfs_dir: &Path) -> Result<u64> {
    // Sum apparent (logical) file sizes via `find … -exec stat -f %z`.
    // This bypasses APFS compression and gives the true byte count.
    let output = Command::new("find")
        .arg(rootfs_dir)
        .args(["!", "-type", "d", "-exec", "stat", "-f", "%z", "{}", "+"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "find/stat (apparent size)".to_string(),
            source: e,
        })?;
    let output = Error::check_command("find/stat (apparent size)", output)?;

    let total_bytes: u64 = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u64>().ok())
        .sum();
    let apparent_mib = total_bytes / (1024 * 1024);

    // Count files+symlinks to estimate block-alignment waste (each file
    // uses at least one 4 KiB block on ext4, regardless of actual size).
    let file_count = String::from_utf8_lossy(&output.stdout).lines().count() as u64;
    let block_waste_mib = file_count * 4 / 1024; // 4 KiB per file → MiB

    // ext4 overhead includes inode tables (significant with -i 8192),
    // journal (128 MiB), superblock copies, and block group descriptors.
    // Since the image file is sparse, generous sizing costs nothing on
    // the host — only actually-written blocks consume APFS space.
    // Use 2x the data estimate to leave ample room.
    let data_mib = apparent_mib + block_waste_mib;
    let total_mib = data_mib * 2;

    Ok(total_mib.max(256))
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
/// If a `fakeroot.state` file exists next to `rootfs_dir` (created during
/// tar extraction), `mkfs.ext4` is run under `fakeroot -i` so it reads
/// the correct ownership metadata instead of the macOS user's uid/gid.
///
/// Uses [`crate::storage::find_e2fsprogs_tool`] to locate `mkfs.ext4`
/// in Homebrew's keg-only installation path.
fn mkfs_ext4_from_dir(image_path: &Path, rootfs_dir: &Path) -> Result<()> {
    let mkfs = crate::storage::find_e2fsprogs_tool("mkfs.ext4");

    let state_file = rootfs_dir
        .parent()
        .expect("rootfs_dir has parent")
        .join("fakeroot.state");

    // Use -i 8192 (bytes-per-inode) to allocate more inodes than the default
    // (16384).  Rootfs trees from dev images contain hundreds of thousands of
    // small files (HTML docs, man pages) that exhaust the default inode table.
    let output = if state_file.exists() {
        Command::new("fakeroot")
            .arg("-i")
            .arg(&state_file)
            .arg("--")
            .arg(&mkfs)
            .args(["-F", "-q", "-i", "8192", "-m", "0"])
            .arg("-d")
            .arg(rootfs_dir)
            .arg(image_path)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "fakeroot mkfs.ext4".to_string(),
                source: e,
            })?
    } else {
        Command::new(&mkfs)
            .args(["-F", "-q", "-i", "8192", "-m", "0"])
            .arg("-d")
            .arg(rootfs_dir)
            .arg(image_path)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "mkfs.ext4".to_string(),
                source: e,
            })?
    };

    Error::check_command("mkfs.ext4", output)?;
    Ok(())
}

/// Shrink an ext4 image to its minimum size.
///
/// Runs `e2fsck -fy` (required before resize) then `resize2fs -M` to shrink
/// the filesystem, and finally `truncate` the file to the new filesystem size.
fn shrink_to_fit(image_path: &Path) -> Result<()> {
    let e2fsck = crate::storage::find_e2fsprogs_tool("e2fsck");
    let resize2fs = crate::storage::find_e2fsprogs_tool("resize2fs");

    // e2fsck -fy: force check, assume yes to all repairs.
    let output = Command::new(&e2fsck)
        .args(["-fy"])
        .arg(image_path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "e2fsck".to_string(),
            source: e,
        })?;
    // e2fsck returns 1 when it fixes errors, which is fine.
    if !output.status.success() && output.status.code() != Some(1) {
        Error::check_command("e2fsck", output)?;
    }

    // resize2fs -M: shrink filesystem to minimum size.
    let output = Command::new(&resize2fs)
        .args(["-M"])
        .arg(image_path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "resize2fs".to_string(),
            source: e,
        })?;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    Error::check_command("resize2fs", output)?;

    // Parse the new block count from resize2fs output to truncate the file.
    // Output looks like: "The filesystem on /path is now 1234567 (4k) blocks long."
    if let Some(new_size) = parse_resize2fs_size(&stderr) {
        let output = Command::new("truncate")
            .args(["-s", &new_size.to_string()])
            .arg(image_path)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "truncate".to_string(),
                source: e,
            })?;
        Error::check_command("truncate (shrink)", output)?;
    }

    Ok(())
}

/// Parse the final filesystem size in bytes from resize2fs stderr.
///
/// Looks for: "The filesystem on <path> is now <blocks> (<size>) blocks long."
fn parse_resize2fs_size(stderr: &str) -> Option<u64> {
    // Find line with "is now N (Xk) blocks long"
    for line in stderr.lines() {
        if let Some(rest) = line.strip_suffix(" blocks long.") {
            // "... is now 1234567 (4k)" → extract block count and block size
            let rest = rest.rsplit(" is now ").next()?;
            let mut parts = rest.split_whitespace();
            let blocks: u64 = parts.next()?.parse().ok()?;
            let block_size_str = parts.next()?.trim_matches(|c| c == '(' || c == ')');
            let block_size: u64 = if block_size_str.ends_with('k') {
                block_size_str.trim_end_matches('k').parse::<u64>().ok()? * 1024
            } else {
                block_size_str.parse().ok()?
            };
            return Some(blocks * block_size);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_resize2fs_4k_blocks() {
        let stderr = "resize2fs 1.47.4 (6-Mar-2025)\n\
            Resizing the filesystem on /tmp/test.ext4 to 1572864 (4k) blocks.\n\
            The filesystem on /tmp/test.ext4 is now 1572864 (4k) blocks long.\n";
        assert_eq!(
            parse_resize2fs_size(stderr),
            Some(1572864 * 4096) // 6 GiB
        );
    }

    #[test]
    fn parse_resize2fs_no_match() {
        assert_eq!(parse_resize2fs_size("some unrelated output"), None);
    }
}
