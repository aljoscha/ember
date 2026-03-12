//! macOS storage backend: APFS copy-on-write clones for disk images.
//!
//! Uses raw `.img` files (ext4) and `cp -c` (APFS CoW clones) for instant
//! VM cloning and snapshots. No ZFS, no root privileges required.
//!
//! Storage layout under the state directory:
//! ```text
//! ~/Library/Application Support/ember/
//! ├── images/data/<name>-<tag>.img      # Base ext4 disk images
//! └── vms/<vm-name>/
//!     ├── rootfs.img                    # APFS clone of base image
//!     └── snapshots/
//!         └── <snap>.img                # APFS clone at snapshot time
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::backend::{InitConfig, SnapshotInfo, StorageBackend};
use crate::config::size::ByteSize;
use crate::error::{Error, Result};

/// macOS storage backend using APFS copy-on-write clones.
///
/// Holds the state directory path, from which all image/VM/snapshot
/// paths are derived.
#[derive(Clone)]
pub struct MacosStorage {
    /// Root state directory (e.g., `~/Library/Application Support/ember`).
    state_dir: PathBuf,
}

impl MacosStorage {
    /// Create a new macOS storage backend from the global config.
    ///
    /// Extracts the state directory path that all storage operations need.
    pub fn new(config: &crate::cli::init::GlobalConfig) -> Self {
        Self {
            state_dir: config.state_dir.clone(),
        }
    }

    /// Path to the images data directory.
    fn images_dir(&self) -> PathBuf {
        self.state_dir.join("images").join("data")
    }

    /// Path to the VMs directory.
    fn vms_dir(&self) -> PathBuf {
        self.state_dir.join("vms")
    }

    /// Path to a specific VM's directory.
    fn vm_dir(&self, vm_name: &str) -> PathBuf {
        self.vms_dir().join(vm_name)
    }

    /// Path to a VM's rootfs disk image.
    fn vm_rootfs(&self, vm_name: &str) -> PathBuf {
        self.vm_dir(vm_name).join("rootfs.img")
    }

    /// Path to a VM's snapshots directory.
    fn vm_snapshots_dir(&self, vm_name: &str) -> PathBuf {
        self.vm_dir(vm_name).join("snapshots")
    }

    /// Path to a base image file.
    fn image_path(&self, name: &str) -> PathBuf {
        self.images_dir().join(format!("{name}.img"))
    }
}

impl StorageBackend for MacosStorage {
    /// Initialize storage directories during `ember init`.
    ///
    /// Creates the directory hierarchy under the state directory:
    /// - `images/data/` for base ext4 disk images
    /// - `vms/` for per-VM directories (created later by clone_for_vm)
    /// - `kernels/` for kernel presets
    /// - `network/` for consistency with Linux (unused on macOS)
    fn init(config: &InitConfig) -> Result<()> {
        let state_dir = &config.state_dir;

        // Validate that the state directory resides on an APFS volume.
        // Warn (don't error) if not — the user might know what they're doing.
        check_apfs_volume(state_dir);

        let dirs = [
            state_dir.join("images").join("data"),
            state_dir.join("vms"),
            state_dir.join("kernels"),
            state_dir.join("network"),
        ];

        for dir in &dirs {
            fs::create_dir_all(dir).map_err(|e| Error::Io {
                path: dir.clone(),
                source: e,
            })?;
            println!("Created {}", dir.display());
        }

        Ok(())
    }

    /// Import an ext4 image file into the images directory.
    ///
    /// On macOS, the raw `.img` file *is* the base image — no zvol, no
    /// `@base` snapshot. The file is simply moved (or copied) into
    /// `images/data/<name>.img`. `size_mib` is unused on macOS.
    fn create_image_volume(
        &self,
        name: &str,
        image_path: &Path,
        _size_mib: u64,
    ) -> Result<PathBuf> {
        let dest = self.image_path(name);

        // Ensure the images directory exists.
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        // Move the image file into place. Use rename if possible (same
        // filesystem), fall back to copy + delete for cross-device moves.
        if fs::rename(image_path, &dest).is_err() {
            fs::copy(image_path, &dest).map_err(|e| Error::Io {
                path: dest.clone(),
                source: e,
            })?;
            let _ = fs::remove_file(image_path);
        }

        Ok(dest)
    }

    /// Clone a base image for a new VM using APFS copy-on-write.
    ///
    /// `cp -c` creates an instant CoW clone — the VM's rootfs shares blocks
    /// with the base image until written to. This is the macOS equivalent of
    /// `zfs clone pool/.../images/name@base pool/.../vms/vm_name`.
    fn clone_for_vm(&self, image_name: &str, vm_name: &str) -> Result<PathBuf> {
        let src = self.image_path(image_name);
        if !src.exists() {
            return Err(Error::Image(format!(
                "base image not found: {}",
                src.display()
            )));
        }

        let vm_dir = self.vm_dir(vm_name);
        fs::create_dir_all(&vm_dir).map_err(|e| Error::Io {
            path: vm_dir.clone(),
            source: e,
        })?;

        // Create snapshots directory for this VM.
        let snap_dir = self.vm_snapshots_dir(vm_name);
        fs::create_dir_all(&snap_dir).map_err(|e| Error::Io {
            path: snap_dir,
            source: e,
        })?;

        let dest = self.vm_rootfs(vm_name);
        apfs_clone(&src, &dest)?;

        Ok(dest)
    }

    /// Create a snapshot by APFS-cloning the VM's current rootfs.
    ///
    /// `cp -c vms/<vm>/rootfs.img → vms/<vm>/snapshots/<snap>.img`
    /// This is instant (CoW) and costs no additional disk space until
    /// the VM's rootfs diverges from the snapshot.
    fn snapshot(&self, vm_name: &str, snap_name: &str) -> Result<()> {
        let src = self.vm_rootfs(vm_name);
        if !src.exists() {
            return Err(Error::Image(format!(
                "VM rootfs not found: {}",
                src.display()
            )));
        }

        let snap_dir = self.vm_snapshots_dir(vm_name);
        fs::create_dir_all(&snap_dir).map_err(|e| Error::Io {
            path: snap_dir.clone(),
            source: e,
        })?;

        let dest = snap_dir.join(format!("{snap_name}.img"));
        if dest.exists() {
            return Err(Error::Image(format!(
                "snapshot '{snap_name}' already exists for VM '{vm_name}'"
            )));
        }

        apfs_clone(&src, &dest)?;
        Ok(())
    }

    /// Restore a snapshot by replacing the VM's rootfs with an APFS clone
    /// of the snapshot file.
    ///
    /// `cp -c vms/<vm>/snapshots/<snap>.img → vms/<vm>/rootfs.img`
    /// The old rootfs is removed first, then replaced with a fresh CoW clone
    /// of the snapshot.
    fn restore_snapshot(&self, vm_name: &str, snap_name: &str) -> Result<()> {
        let snap_path = self
            .vm_snapshots_dir(vm_name)
            .join(format!("{snap_name}.img"));
        if !snap_path.exists() {
            return Err(Error::Image(format!(
                "snapshot '{snap_name}' not found for VM '{vm_name}'"
            )));
        }

        let rootfs = self.vm_rootfs(vm_name);

        // Clone to a temporary file first, then atomically rename.
        // This prevents data loss if the clone fails — the original rootfs
        // is only replaced once the new clone is fully written.
        let tmp_rootfs = rootfs.with_extension("img.restoring");
        if tmp_rootfs.exists() {
            fs::remove_file(&tmp_rootfs).map_err(|e| Error::Io {
                path: tmp_rootfs.clone(),
                source: e,
            })?;
        }

        apfs_clone(&snap_path, &tmp_rootfs)?;

        // Atomic rename replaces the old rootfs in one operation.
        fs::rename(&tmp_rootfs, &rootfs).map_err(|e| Error::Io {
            path: rootfs,
            source: e,
        })?;
        Ok(())
    }

    /// Delete a snapshot by removing its image file.
    ///
    /// APFS reference-counts the underlying blocks — deleting a snapshot only
    /// frees blocks that are not shared with other clones (rootfs or other
    /// snapshots).
    fn delete_snapshot(&self, vm_name: &str, snap_name: &str) -> Result<()> {
        let snap_path = self
            .vm_snapshots_dir(vm_name)
            .join(format!("{snap_name}.img"));
        if !snap_path.exists() {
            return Err(Error::Image(format!(
                "snapshot '{snap_name}' not found for VM '{vm_name}'"
            )));
        }

        fs::remove_file(&snap_path).map_err(|e| Error::Io {
            path: snap_path,
            source: e,
        })?;
        Ok(())
    }

    /// List all snapshots for a VM by reading the `snapshots/` directory.
    ///
    /// Each `.img` file in the directory is a snapshot. Metadata (creation
    /// time, size) comes from `fs::metadata` on each file.
    fn list_snapshots(&self, vm_name: &str) -> Result<Vec<SnapshotInfo>> {
        let snap_dir = self.vm_snapshots_dir(vm_name);
        if !snap_dir.exists() {
            return Ok(vec![]);
        }

        let mut snapshots = Vec::new();
        let entries = fs::read_dir(&snap_dir).map_err(|e| Error::Io {
            path: snap_dir.clone(),
            source: e,
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| Error::Io {
                path: snap_dir.clone(),
                source: e,
            })?;
            let path = entry.path();

            // Only consider .img files as snapshots.
            if path.extension().and_then(|e| e.to_str()) != Some("img") {
                continue;
            }

            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();

            let meta = fs::metadata(&path).map_err(|e| Error::Io {
                path: path.clone(),
                source: e,
            })?;

            // Use file modification time as creation timestamp. On macOS,
            // the birth time (created) would be more accurate, but mtime
            // is portable and close enough — the file is created once and
            // never modified.
            let created_at = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);

            snapshots.push(SnapshotInfo {
                name,
                created_at,
                size: meta.len(),
            });
        }

        // Sort by creation time (oldest first) for consistent output.
        snapshots.sort_by_key(|s| s.created_at);
        Ok(snapshots)
    }

    /// Resize a VM's rootfs image.
    ///
    /// 1. Grow the raw `.img` file with `truncate` to the new size.
    /// 2. Run `e2fsck -f` to ensure filesystem consistency before resize.
    /// 3. Run `resize2fs` to expand the ext4 filesystem to fill the image.
    ///
    /// Only growing is supported — the CLI layer prevents shrink attempts.
    /// Requires `e2fsprogs` from Homebrew (`brew install e2fsprogs`).
    fn resize(&self, vm_name: &str, new_size: ByteSize) -> Result<()> {
        let rootfs = self.vm_rootfs(vm_name);
        if !rootfs.exists() {
            return Err(Error::Image(format!(
                "VM rootfs not found: {}",
                rootfs.display()
            )));
        }

        // Grow the raw image file to the new size.
        let output = Command::new("truncate")
            .arg("-s")
            .arg(new_size.bytes().to_string())
            .arg(&rootfs)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "truncate".to_string(),
                source: e,
            })?;
        Error::check_command("truncate", output)?;

        // Check filesystem consistency before resizing (resize2fs requires this).
        // e2fsprogs tools are installed via Homebrew and may not be in PATH.
        let e2fsck = find_e2fsprogs_tool("e2fsck");
        let output = Command::new(&e2fsck)
            .arg("-f") // force check even if clean
            .arg("-y") // auto-fix errors
            .arg(&rootfs)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "e2fsck".to_string(),
                source: e,
            })?;
        // e2fsck exit codes are a bitmask: bit 0 (1) = errors corrected (OK with -y),
        // bit 1 (2) = reboot needed, bit 2 (4) = errors uncorrected, bit 3 (8) = operational error.
        // Only treat exit >= 2 as failure, matching the Linux backend.
        let code = output.status.code().unwrap_or(-1);
        if code >= 2 {
            Error::check_command("e2fsck", output)?;
        }

        // Expand the ext4 filesystem to fill the (now larger) image file.
        let resize2fs = find_e2fsprogs_tool("resize2fs");
        let output = Command::new(&resize2fs)
            .arg(&rootfs)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "resize2fs".to_string(),
                source: e,
            })?;
        Error::check_command("resize2fs", output)?;

        Ok(())
    }

    /// Destroy all storage for a VM: rootfs image, snapshots, and VM directory.
    ///
    /// Silently succeeds if the directory doesn't exist (idempotent delete).
    fn destroy_vm_storage(&self, vm_name: &str) -> Result<()> {
        let vm_dir = self.vm_dir(vm_name);
        if vm_dir.exists() {
            fs::remove_dir_all(&vm_dir).map_err(|e| Error::Io {
                path: vm_dir,
                source: e,
            })?;
        }
        Ok(())
    }

    /// Destroy storage for a base image (the raw `.img` file).
    fn destroy_image_storage(&self, name: &str) -> Result<()> {
        let img = self.image_path(name);
        if img.exists() {
            fs::remove_file(&img).map_err(|e| Error::Io {
                path: img,
                source: e,
            })?;
        }
        Ok(())
    }

    /// Path to a VM's rootfs disk image (used as the virtio-blk device).
    ///
    /// On macOS the raw `.img` file is passed directly to AVF — no
    /// block device indirection like ZFS zvols.
    fn disk_device_path(&self, vm_name: &str) -> PathBuf {
        self.vm_rootfs(vm_name)
    }

    /// Clone a source VM's state for forking.
    ///
    /// Creates a snapshot of the source VM (APFS clone of its rootfs),
    /// then clones that snapshot into the target VM's rootfs.
    /// Returns `(target_rootfs_path, fork_origin_identifier)`.
    fn clone_from_snapshot(
        &self,
        source_vm: &str,
        snap_name: &str,
        target_vm: &str,
    ) -> Result<(PathBuf, String)> {
        // Create the snapshot on the source VM.
        self.snapshot(source_vm, snap_name)?;

        // Create target VM directory and snapshots subdirectory.
        let target_dir = self.vm_dir(target_vm);
        fs::create_dir_all(&target_dir).map_err(|e| Error::Io {
            path: target_dir.clone(),
            source: e,
        })?;
        let snap_dir = self.vm_snapshots_dir(target_vm);
        fs::create_dir_all(&snap_dir).map_err(|e| Error::Io {
            path: snap_dir,
            source: e,
        })?;

        // Clone the snapshot into the target VM's rootfs.
        let snap_path = self
            .vm_snapshots_dir(source_vm)
            .join(format!("{snap_name}.img"));
        let target_rootfs = self.vm_rootfs(target_vm);

        if let Err(e) = apfs_clone(&snap_path, &target_rootfs) {
            // Clean up the snapshot on failure.
            let _ = self.delete_snapshot(source_vm, snap_name);
            return Err(e);
        }

        // Fork origin identifier: "source_vm/snap_name" so we can find
        // and delete the snapshot when the forked VM is deleted.
        let fork_origin = format!("{source_vm}/{snap_name}");
        Ok((target_rootfs, fork_origin))
    }

    /// Clean up the fork origin snapshot created by [`clone_from_snapshot`].
    ///
    /// The fork_origin string is "source_vm/snap_name". We parse it and
    /// delete the snapshot file. Errors are logged but not propagated
    /// (same behavior as the Linux backend).
    fn destroy_fork_origin(&self, fork_origin: &str) -> Result<()> {
        if let Some((source_vm, snap_name)) = fork_origin.split_once('/') {
            match self.delete_snapshot(source_vm, snap_name) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("Warning: failed to clean up fork snapshot '{fork_origin}': {e}");
                }
            }
        }
        Ok(())
    }

    /// Not supported for ext4 on macOS.
    ///
    /// macOS has no native ext4 mount support. Use [`inject_ssh_key`] for
    /// the primary use case (SSH key injection during VM creation).
    fn mount(&self, _path: &Path) -> Result<PathBuf> {
        Err(Error::Image(
            "ext4 mounting is not supported on macOS — \
             macOS has no native ext4 filesystem support"
                .to_string(),
        ))
    }

    /// Not supported for ext4 on macOS (see [`mount`]).
    fn unmount(&self, _mount_point: &Path) -> Result<()> {
        Err(Error::Image(
            "ext4 unmounting is not supported on macOS".to_string(),
        ))
    }

    /// Inject an SSH public key into a VM's ext4 rootfs image using `debugfs`.
    ///
    /// macOS can't mount ext4 natively, so we use `debugfs -w` from Homebrew
    /// e2fsprogs to write files directly into the ext4 image:
    ///
    /// 1. `debugfs -R 'stat /home/ubuntu'` — detect SSH user
    /// 2. `debugfs -w` — create `.ssh/` dir and write `authorized_keys`
    /// 3. `set_inode_field` — fix permissions (700/.ssh, 600/authorized_keys)
    ///    and ownership (uid/gid matching the target user)
    fn inject_ssh_key(&self, image_path: &Path, pubkey_path: &Path) -> Result<String> {
        let debugfs = find_e2fsprogs_tool("debugfs");

        // Step 1: Detect SSH user and read uid/gid from the image.
        // Check if /home/ubuntu exists; if so, read its owner from the inode.
        // This avoids hardcoding uid/gid which may differ across images.
        let output = Command::new(&debugfs)
            .args(["-R", "stat /home/ubuntu"])
            .arg(image_path)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "debugfs stat".to_string(),
                source: e,
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let has_ubuntu = !stderr.contains("File not found");

        let (ssh_user, home_path, uid, gid) = if has_ubuntu {
            // Parse uid/gid from the /etc/passwd entry for the ubuntu user.
            // Fall back to reading the home directory inode ownership, and
            // ultimately to 1000:1000 as a last resort.
            let (uid, gid) = read_passwd_uid_gid(Path::new(&debugfs), image_path, "ubuntu")
                .or_else(|| parse_debugfs_uid_gid(&stdout))
                .unwrap_or((1000, 1000));
            ("ubuntu", "/home/ubuntu", uid, gid)
        } else {
            ("root", "/root", 0u32, 0u32)
        };

        let ssh_dir = format!("{home_path}/.ssh");
        let ak_path = format!("{ssh_dir}/authorized_keys");

        // Step 2: Write SSH key using debugfs.
        // The `write` command copies a host file into the ext4 image.
        // `set_inode_field` fixes permissions and ownership.
        let pubkey_abs = std::fs::canonicalize(pubkey_path).map_err(|e| Error::Io {
            path: pubkey_path.to_path_buf(),
            source: e,
        })?;

        let commands = format!(
            "mkdir {ssh_dir}\n\
             write {pubkey} {ak_path}\n\
             set_inode_field {ssh_dir} mode 040700\n\
             set_inode_field {ssh_dir} uid {uid}\n\
             set_inode_field {ssh_dir} gid {gid}\n\
             set_inode_field {ak_path} mode 0100600\n\
             set_inode_field {ak_path} uid {uid}\n\
             set_inode_field {ak_path} gid {gid}\n",
            pubkey = pubkey_abs.display(),
        );

        // Write commands to a temp file and pass to debugfs -f.
        let cmd_file = tempfile::NamedTempFile::new().map_err(|e| Error::Io {
            path: std::env::temp_dir(),
            source: e,
        })?;
        std::fs::write(cmd_file.path(), &commands).map_err(|e| Error::Io {
            path: cmd_file.path().to_path_buf(),
            source: e,
        })?;

        let output = Command::new(&debugfs)
            .arg("-w")
            .arg("-f")
            .arg(cmd_file.path())
            .arg(image_path)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "debugfs write".to_string(),
                source: e,
            })?;

        // debugfs exits 0 even on some errors; check stderr for real failures.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such file or directory")
            && !stderr.contains("File not found by ext2_lookup")
        {
            return Err(Error::Image(format!(
                "debugfs failed to inject SSH key: {stderr}"
            )));
        }

        Ok(ssh_user.to_string())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Read a user's uid and gid from `/etc/passwd` inside an ext4 image via debugfs.
///
/// Returns `None` if the user isn't found or the file can't be read.
fn read_passwd_uid_gid(debugfs: &Path, image_path: &Path, username: &str) -> Option<(u32, u32)> {
    let output = Command::new(debugfs)
        .args(["-R", "cat /etc/passwd"])
        .arg(image_path)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // /etc/passwd format: name:x:uid:gid:...
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 4 && fields[0] == username {
            let uid = fields[2].parse::<u32>().ok()?;
            let gid = fields[3].parse::<u32>().ok()?;
            return Some((uid, gid));
        }
    }
    None
}

/// Parse uid and gid from `debugfs stat` output.
///
/// Looks for the `User: <uid>   Group: <gid>` line in debugfs stat output.
fn parse_debugfs_uid_gid(stat_output: &str) -> Option<(u32, u32)> {
    for line in stat_output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("User:") {
            // Format: "User:  1000   Group:  1000   Project: ..."
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 3 && parts[1] == "Group:" {
                let uid = parts[0].parse::<u32>().ok()?;
                let gid = parts[2].parse::<u32>().ok()?;
                return Some((uid, gid));
            }
        }
    }
    None
}

/// Create an APFS copy-on-write clone using `cp -c`.
///
/// This is instant regardless of file size — APFS shares the underlying
/// blocks between source and destination. Only blocks that are subsequently
/// modified will be allocated separately.
///
/// `cp -c` fails with a clear error (rather than silently falling back to
/// a full copy) if CoW isn't possible:
/// - Cross-volume: "clonefile failed: Cross-device link"
/// - Non-APFS: "clonefile failed: Not supported"
fn apfs_clone(src: &Path, dest: &Path) -> Result<()> {
    let start = Instant::now();

    let output = Command::new("cp")
        .arg("-c")
        .arg(src)
        .arg(dest)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "cp -c".to_string(),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Provide a clear error message for common APFS clone failures.
        let msg = if stderr.contains("Cross-device link") || stderr.contains("Not supported") {
            format!(
                "APFS clone failed: {}. \
                 VM storage must be on an APFS volume. The state directory may be on \
                 a non-APFS filesystem or the source and destination are on different volumes.",
                stderr.trim()
            )
        } else {
            format!(
                "cp -c {} → {} failed: {}",
                src.display(),
                dest.display(),
                stderr.trim()
            )
        };
        return Err(Error::Image(msg));
    }

    // A CoW clone completes in milliseconds regardless of file size.
    // If it takes over 1 second, something may be wrong (e.g., falling
    // back to a full copy on a non-APFS volume that doesn't error).
    let elapsed = start.elapsed();
    if elapsed.as_secs() >= 1 {
        eprintln!(
            "Warning: disk clone took {:.1}s — this may indicate copy-on-write is not working. \
             Run `ember debug storage-efficiency` to check.",
            elapsed.as_secs_f64()
        );
    }

    Ok(())
}

/// Find an e2fsprogs tool (e2fsck, resize2fs, mkfs.ext4) by checking
/// common Homebrew installation paths before falling back to PATH.
///
/// Homebrew installs e2fsprogs as keg-only (not symlinked into /usr/local/bin
/// or /opt/homebrew/bin) because macOS ships its own fsck. The sbin/ directory
/// under the Homebrew prefix contains the actual binaries.
pub(crate) fn find_e2fsprogs_tool(name: &str) -> String {
    // Apple Silicon Homebrew prefix.
    let arm_path = format!("/opt/homebrew/opt/e2fsprogs/sbin/{name}");
    if Path::new(&arm_path).exists() {
        return arm_path;
    }
    // Intel Homebrew prefix.
    let intel_path = format!("/usr/local/opt/e2fsprogs/sbin/{name}");
    if Path::new(&intel_path).exists() {
        return intel_path;
    }
    // Fall back to PATH lookup.
    name.to_string()
}

/// Check whether the given path resides on an APFS volume.
///
/// Runs `diskutil info <path>` and looks for `File System Personality: APFS`
/// in the output. Prints a warning if the volume is not APFS (cloning will
/// fail or be slow). Silently returns if the check can't be performed
/// (e.g., `diskutil` not available or path doesn't exist yet).
fn check_apfs_volume(path: &Path) {
    // Use the path itself (or its nearest existing ancestor) for the check.
    let check_path = {
        let mut p = path.to_path_buf();
        while !p.exists() {
            if !p.pop() {
                return; // Can't find an existing ancestor — skip check.
            }
        }
        p
    };

    let output = match Command::new("diskutil")
        .args(["info", "-plist"])
        .arg(&check_path)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return, // Can't run diskutil — skip check silently.
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Look for <key>FilesystemType</key> followed by <string>apfs</string>.
    // The value is lowercase in plist output.
    let is_apfs = stdout
        .find("<key>FilesystemType</key>")
        .and_then(|idx| {
            let after = &stdout[idx..];
            let start = after.find("<string>")? + "<string>".len();
            let end = after.find("</string>")?;
            Some(after[start..end].trim().to_lowercase())
        })
        .map(|fs_type| fs_type == "apfs")
        .unwrap_or(false);

    if !is_apfs {
        eprintln!(
            "Warning: {} is not on an APFS volume. \
             Copy-on-write clones (cp -c) will not work, and VM cloning \
             will use full copies instead of instant CoW clones.",
            path.display()
        );
    }
}
