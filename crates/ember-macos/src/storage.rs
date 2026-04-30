//! macOS storage backend: APFS copy-on-write clones for disk images.
//!
//! Uses raw `.img` files (ext4) and `cp -c` (APFS CoW clones) for instant
//! VM cloning. No ZFS, no root privileges required.
//!
//! Storage layout under the state directory:
//! ```text
//! ~/Library/Application Support/ember/
//! ├── images/data/<name>-<tag>.img      # Base ext4 disk images
//! └── vms/<vm-name>/
//!     └── rootfs.img                    # APFS clone of base image
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use ember_core::backend::{InitConfig, StorageBackend, VolumeHandle};
use ember_core::config::size::ByteSize;
use ember_core::error::{Error, Result};
use ember_core::image::registry::ImageEntry;
use ember_core::state::vm::VmMetadata;

/// macOS storage backend using APFS copy-on-write clones.
///
/// Holds the state directory path, from which all image/VM paths are
/// derived.
#[derive(Clone)]
pub struct MacosStorage {
    /// Root state directory (e.g., `~/Library/Application Support/ember`).
    state_dir: PathBuf,
}

impl MacosStorage {
    /// Create a new macOS storage backend from the global config.
    ///
    /// Extracts the state directory path that all storage operations need.
    pub fn new(config: &ember_core::config::GlobalConfig) -> Self {
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
    ) -> Result<VolumeHandle> {
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

        Ok(VolumeHandle::from_path(dest))
    }

    /// Clone a base image for a new VM using APFS copy-on-write.
    ///
    /// `cp -c` creates an instant CoW clone — the VM's rootfs shares blocks
    /// with the base image until written to. This is the macOS equivalent of
    /// `zfs clone pool/.../images/name@base pool/.../vms/vm_name`.
    fn clone_for_vm(&self, image: &ImageEntry, vm_name: &str) -> Result<VolumeHandle> {
        let src = self.image_path(&image.local_name);
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

        let dest = self.vm_rootfs(vm_name);
        apfs_clone(&src, &dest)?;

        Ok(VolumeHandle::from_path(dest))
    }

    /// Resize a VM's rootfs image.
    ///
    /// 1. Grow the raw `.img` file with `truncate` to the new size.
    /// 2. Run `e2fsck -f` to ensure filesystem consistency before resize.
    /// 3. Run `resize2fs` to expand the ext4 filesystem to fill the image.
    ///
    /// Only growing is supported — the CLI layer prevents shrink attempts.
    /// Requires `e2fsprogs` from Homebrew (`brew install e2fsprogs`).
    fn resize(&self, vm: &VmMetadata, new_size: ByteSize) -> Result<()> {
        let rootfs = self.vm_rootfs(&vm.name);
        if !rootfs.exists() {
            return Err(Error::Image(format!(
                "VM rootfs not found: {}",
                rootfs.display()
            )));
        }

        // Defensive guard: refuse to shrink the image.
        let current_size = std::fs::metadata(&rootfs)
            .map_err(|e| Error::Io {
                path: rootfs.clone(),
                source: e,
            })?
            .len();
        if new_size.bytes() <= current_size {
            return Err(Error::Image(format!(
                "cannot shrink disk from {} to {} bytes",
                current_size,
                new_size.bytes()
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

    /// Destroy all storage for a VM: rootfs image and VM directory.
    ///
    /// Silently succeeds if the directory doesn't exist (idempotent delete).
    fn destroy_vm_storage(&self, vm: &VmMetadata) -> Result<()> {
        let vm_dir = self.vm_dir(&vm.name);
        if vm_dir.exists() {
            fs::remove_dir_all(&vm_dir).map_err(|e| Error::Io {
                path: vm_dir,
                source: e,
            })?;
        }
        Ok(())
    }

    /// Destroy storage for a base image (the raw `.img` file).
    /// The `force` flag is a no-op on macOS (APFS clones are independent).
    fn destroy_image_storage(&self, image: &ImageEntry, _force: bool) -> Result<()> {
        let img = self.image_path(&image.local_name);
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
    fn disk_device_path(&self, vm: &VmMetadata) -> Result<PathBuf> {
        Ok(self.vm_rootfs(&vm.name))
    }

    /// Clone a source VM's disk for forking via APFS copy-on-write.
    ///
    /// Directly clones the source VM's rootfs into the target VM's rootfs
    /// using `cp -c`. APFS clones are fully independent, so no cleanup
    /// or dependency tracking is needed.
    fn clone_vm_storage(&self, source: &VmMetadata, target_vm: &str) -> Result<VolumeHandle> {
        let source_rootfs = self.vm_rootfs(&source.name);
        if !source_rootfs.exists() {
            return Err(Error::Image(format!(
                "source VM rootfs not found: {}",
                source_rootfs.display()
            )));
        }

        let target_dir = self.vm_dir(target_vm);
        fs::create_dir_all(&target_dir).map_err(|e| Error::Io {
            path: target_dir.clone(),
            source: e,
        })?;

        let target_rootfs = self.vm_rootfs(target_vm);
        apfs_clone(&source_rootfs, &target_rootfs)?;

        Ok(VolumeHandle::from_path(target_rootfs))
    }

    /// No-op on macOS — APFS clones are independent, nothing to clean up.
    fn cleanup_fork(&self, _parent: &VmMetadata, _forked: &VmMetadata) -> Result<()> {
        Ok(())
    }

    /// Always returns empty on macOS — APFS clones are independent.
    fn storage_dependents(&self, _vm: &VmMetadata) -> Result<Vec<String>> {
        Ok(vec![])
    }

    fn deinit(&self, purge: bool) -> Result<()> {
        // The state directory layout (`images/`, `vms/`, `kernels/`,
        // `network/`) is owned by ember; on `--purge` we drop the disk
        // images so a future `ember init` starts clean.
        if purge {
            let images = self.images_dir();
            if images.exists() {
                fs::remove_dir_all(&images).map_err(|e| Error::Io {
                    path: images,
                    source: e,
                })?;
            }
            let vms = self.vms_dir();
            if vms.exists() {
                fs::remove_dir_all(&vms).map_err(|e| Error::Io {
                    path: vms,
                    source: e,
                })?;
            }
        }
        Ok(())
    }

    fn grow(&self, _new_size: ByteSize) -> Result<()> {
        Err(Error::Image(
            "macOS/APFS has no pool concept — resize individual VMs with \
             `ember vm resize` instead"
                .to_string(),
        ))
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

        // Check exit code first.
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Image(format!(
                "debugfs exited with {}: {stderr}",
                output.status
            )));
        }

        // Verify the authorized_keys file was actually written.
        // This is more robust than parsing stderr strings, which vary by
        // debugfs version. If the file doesn't exist after the write,
        // something went wrong regardless of what stderr says.
        let verify = Command::new(&debugfs)
            .args(["-R", &format!("stat {ak_path}")])
            .arg(image_path)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "debugfs verify".to_string(),
                source: e,
            })?;
        let verify_stderr = String::from_utf8_lossy(&verify.stderr);
        if verify_stderr.contains("File not found") {
            let write_stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Image(format!(
                "debugfs SSH key injection failed — authorized_keys not found after write.\n\
                 debugfs stderr: {write_stderr}"
            )));
        }

        Ok(ssh_user.to_string())
    }

    /// Inject the VM's hostname into `/etc/hosts` using `debugfs`.
    ///
    /// Writes a hosts file with loopback entries including the VM name,
    /// then uses `debugfs -w` to replace `/etc/hosts` in the ext4 image.
    fn inject_hostname(&self, image_path: &Path, hostname: &str) -> Result<()> {
        let debugfs = find_e2fsprogs_tool("debugfs");

        // Write the hosts content to a temp file for debugfs to read.
        let hosts_content = format!(
            "127.0.0.1\tlocalhost {hostname}\n\
             ::1\t\tlocalhost ip6-localhost ip6-loopback {hostname}\n"
        );
        let hosts_file = tempfile::NamedTempFile::new().map_err(|e| Error::Io {
            path: std::env::temp_dir(),
            source: e,
        })?;
        std::fs::write(hosts_file.path(), &hosts_content).map_err(|e| Error::Io {
            path: hosts_file.path().to_path_buf(),
            source: e,
        })?;

        // Remove the existing /etc/hosts first, then write the new one.
        let commands = format!(
            "rm /etc/hosts\nwrite {} /etc/hosts\n",
            hosts_file.path().display(),
        );
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
                command: "debugfs write /etc/hosts".to_string(),
                source: e,
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Image(format!(
                "debugfs /etc/hosts injection exited with {}: {stderr}",
                output.status
            )));
        }

        Ok(())
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
