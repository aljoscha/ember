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

        // Remove current rootfs before cloning, so cp -c doesn't fail on
        // an existing destination file.
        if rootfs.exists() {
            fs::remove_file(&rootfs).map_err(|e| Error::Io {
                path: rootfs.clone(),
                source: e,
            })?;
        }

        apfs_clone(&snap_path, &rootfs)?;
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

    fn resize(&self, _vm_name: &str, _new_size: ByteSize) -> Result<()> {
        todo!("macOS: resize")
    }

    fn destroy_vm_storage(&self, _vm_name: &str) -> Result<()> {
        todo!("macOS: destroy_vm_storage")
    }

    fn destroy_image_storage(&self, _name: &str) -> Result<()> {
        todo!("macOS: destroy_image_storage")
    }

    fn disk_device_path(&self, _vm_name: &str) -> PathBuf {
        todo!("macOS: disk_device_path")
    }

    fn clone_from_snapshot(
        &self,
        _source_vm: &str,
        _snap_name: &str,
        _target_vm: &str,
    ) -> Result<(PathBuf, String)> {
        todo!("macOS: clone_from_snapshot")
    }

    fn destroy_fork_origin(&self, _fork_origin: &str) -> Result<()> {
        todo!("macOS: destroy_fork_origin")
    }

    fn mount(&self, _path: &Path) -> Result<PathBuf> {
        todo!("macOS: mount")
    }

    fn unmount(&self, _mount_point: &Path) -> Result<()> {
        todo!("macOS: unmount")
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

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

    Ok(())
}
