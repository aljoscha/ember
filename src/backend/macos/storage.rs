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

    fn clone_for_vm(&self, _image_name: &str, _vm_name: &str) -> Result<PathBuf> {
        todo!("macOS: clone_for_vm")
    }

    fn snapshot(&self, _vm_name: &str, _snap_name: &str) -> Result<()> {
        todo!("macOS: snapshot")
    }

    fn restore_snapshot(&self, _vm_name: &str, _snap_name: &str) -> Result<()> {
        todo!("macOS: restore_snapshot")
    }

    fn delete_snapshot(&self, _vm_name: &str, _snap_name: &str) -> Result<()> {
        todo!("macOS: delete_snapshot")
    }

    fn list_snapshots(&self, _vm_name: &str) -> Result<Vec<SnapshotInfo>> {
        todo!("macOS: list_snapshots")
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
