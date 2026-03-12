//! Linux storage backend: ZFS zvols, snapshots, and clones.
//!
//! Wraps the `zfs::pool`, `zfs::dataset`, `zfs::volume`, and `zfs::snapshot`
//! modules behind the [`StorageBackend`] trait. On Linux, each VM's rootfs
//! is a ZFS zvol cloned from an image zvol's `@base` snapshot.
//!
//! The struct holds the ZFS dataset paths (derived from [`GlobalConfig`]) so
//! trait methods can construct full zvol paths from short names.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use crate::backend::{InitConfig, SnapshotInfo, StorageBackend};
use crate::cli::init::GlobalConfig;
use crate::config::size::ByteSize;
use crate::error::{Error, Result};
use crate::image;
use crate::zfs;

/// Linux storage backend using ZFS zvols.
#[derive(Clone)]
pub struct LinuxStorage {
    /// ZFS images dataset path (e.g., "tank/ember/images").
    images_dataset: String,
    /// ZFS VMs dataset path (e.g., "tank/ember/vms").
    vms_dataset: String,
}

impl LinuxStorage {
    /// Create a new Linux storage backend from the global config.
    ///
    /// Extracts the ZFS pool/dataset paths that all storage operations need.
    pub fn new(config: &GlobalConfig) -> Self {
        Self {
            images_dataset: config.images_dataset(),
            vms_dataset: config.vms_dataset(),
        }
    }

    /// Full ZFS zvol path for an image (e.g., "tank/ember/images/library-alpine-latest").
    fn image_zvol(&self, name: &str) -> String {
        format!("{}/{name}", self.images_dataset)
    }

    /// Full ZFS zvol path for a VM (e.g., "tank/ember/vms/myvm").
    fn vm_zvol(&self, vm_name: &str) -> String {
        format!("{}/{vm_name}", self.vms_dataset)
    }
}

impl StorageBackend for LinuxStorage {
    /// Create or verify ZFS pool and datasets during `ember init`.
    ///
    /// Handles the full ZFS initialization: creates the pool if it doesn't
    /// exist (requires `device`), then creates the dataset hierarchy.
    fn init(config: &InitConfig) -> Result<()> {
        let pool = &config.pool;

        // 1. Create or verify ZFS pool.
        if zfs::pool::exists(pool)? {
            let info = zfs::pool::status(pool)?;
            println!("Pool '{pool}' already exists (health: {})", info.health);
        } else {
            let device = config.device.as_deref().ok_or_else(|| {
                Error::Zfs(format!(
                    "pool '{pool}' does not exist — provide --device to create it"
                ))
            })?;
            println!("Creating ZFS pool '{pool}' on {device}...");
            zfs::pool::create(pool, device)?;
            println!("Pool '{pool}' created.");
        }

        // 2. Create datasets: <pool>/<dataset>, <pool>/<dataset>/images, <pool>/<dataset>/vms.
        let base = format!("{pool}/{}", config.dataset);
        let images = format!("{base}/images");
        let vms = format!("{base}/vms");

        for ds in [&base, &images, &vms] {
            if zfs::dataset::exists(ds)? {
                println!("Dataset '{ds}' already exists.");
            } else {
                println!("Creating dataset '{ds}'...");
                zfs::dataset::create(ds)?;
            }
        }

        Ok(())
    }

    /// Create a ZFS zvol from an ext4 image, write it via `dd`, and snapshot `@base`.
    ///
    /// Returns the zvol path (e.g., "tank/ember/images/library-alpine-latest").
    fn create_image_volume(&self, name: &str, image_path: &Path, size_mib: u64) -> Result<PathBuf> {
        let zvol = self.image_zvol(name);

        // Create the zvol.
        zfs::volume::create(&zvol, size_mib)?;

        // Write the ext4 image to the zvol and create @base snapshot.
        // On failure, clean up the zvol.
        if let Err(e) = image::zvol::write_to_zvol(image_path, &zvol) {
            let _ = zfs::volume::destroy(&zvol, true);
            return Err(e);
        }

        Ok(PathBuf::from(zvol))
    }

    /// Clone the image's `@base` snapshot to create a VM zvol.
    ///
    /// Returns the zvol path (e.g., "tank/ember/vms/myvm").
    fn clone_for_vm(&self, image_name: &str, vm_name: &str) -> Result<PathBuf> {
        let image_zvol = self.image_zvol(image_name);
        let snapshot = format!("{image_zvol}@{}", zfs::BASE_SNAPSHOT_NAME);
        let vm_zvol = self.vm_zvol(vm_name);

        // Verify the @base snapshot exists.
        if !zfs::snapshot::exists(&image_zvol, zfs::BASE_SNAPSHOT_NAME)? {
            return Err(Error::Zfs(format!(
                "image zvol '{image_zvol}' has no @{} snapshot — the image may be corrupted",
                zfs::BASE_SNAPSHOT_NAME
            )));
        }

        zfs::volume::clone(&snapshot, &vm_zvol)?;
        Ok(PathBuf::from(vm_zvol))
    }

    fn snapshot(&self, vm_name: &str, snap_name: &str) -> Result<()> {
        let zvol = self.vm_zvol(vm_name);
        zfs::snapshot::create(&zvol, snap_name)
    }

    fn restore_snapshot(&self, vm_name: &str, snap_name: &str) -> Result<()> {
        let zvol = self.vm_zvol(vm_name);
        zfs::snapshot::rollback(&zvol, snap_name)
    }

    fn delete_snapshot(&self, vm_name: &str, snap_name: &str) -> Result<()> {
        let zvol = self.vm_zvol(vm_name);
        zfs::snapshot::destroy(&zvol, snap_name)
    }

    /// List snapshots, filtering out the reserved `@base` snapshot.
    fn list_snapshots(&self, vm_name: &str) -> Result<Vec<SnapshotInfo>> {
        let zvol = self.vm_zvol(vm_name);
        let zfs_snaps = zfs::snapshot::list(&zvol)?;

        Ok(zfs_snaps
            .into_iter()
            .filter(|s| s.short_name != zfs::BASE_SNAPSHOT_NAME)
            .map(|s| SnapshotInfo {
                name: s.short_name,
                created_at: s.creation,
                size: s.referenced,
            })
            .collect())
    }

    /// Grow the zvol and expand the ext4 filesystem.
    fn resize(&self, vm_name: &str, new_size: ByteSize) -> Result<()> {
        let zvol = self.vm_zvol(vm_name);
        let new_gib = new_size
            .to_gib()
            .map_err(|e| Error::Zfs(format!("invalid resize target: {e}")))?;

        zfs::volume::set_volsize(&zvol, new_gib)?;

        // Wait for the device node to settle after resize, then expand ext4.
        let dev_path = zfs::volume::device_path(&zvol);
        image::zvol::wait_for_device(&dev_path)?;
        e2fsck(&dev_path)?;
        resize2fs(&dev_path)?;

        Ok(())
    }

    /// Destroy the VM's zvol and all its snapshots.
    fn destroy_vm_storage(&self, vm_name: &str) -> Result<()> {
        let zvol = self.vm_zvol(vm_name);
        // Ignore errors — the zvol may already be gone.
        let _ = zfs::volume::destroy(&zvol, true);
        Ok(())
    }

    /// Destroy the image zvol (includes its @base snapshot).
    fn destroy_image_storage(&self, name: &str) -> Result<()> {
        let zvol = self.image_zvol(name);
        zfs::volume::destroy(&zvol, true)
    }

    /// Device path for a VM's root disk zvol.
    ///
    /// Returns the `/dev/zvol/...` path that can be used for mounting
    /// or passing to Firecracker as a block device.
    fn disk_device_path(&self, vm_name: &str) -> PathBuf {
        let zvol = self.vm_zvol(vm_name);
        zfs::volume::device_path(&zvol)
    }

    /// Fork a VM's disk by snapshotting the source and cloning into a new VM.
    ///
    /// Returns `(disk_path, fork_snapshot_full_name)`.
    fn clone_from_snapshot(
        &self,
        source_vm: &str,
        snap_name: &str,
        target_vm: &str,
    ) -> Result<(PathBuf, String)> {
        let source_zvol = self.vm_zvol(source_vm);
        let target_zvol = self.vm_zvol(target_vm);

        // Create the snapshot on the source VM.
        zfs::snapshot::create(&source_zvol, snap_name)?;

        let fork_snap_full = format!("{source_zvol}@{snap_name}");

        // Clone the snapshot into the target VM's zvol.
        if let Err(e) = zfs::volume::clone(&fork_snap_full, &target_zvol) {
            // Clean up the snapshot on failure.
            let _ = zfs::snapshot::destroy(&source_zvol, snap_name);
            return Err(e);
        }

        Ok((PathBuf::from(target_zvol), fork_snap_full))
    }

    /// Clean up a fork origin snapshot (e.g., "tank/ember/vms/source@fork-target").
    fn destroy_fork_origin(&self, fork_origin: &str) -> Result<()> {
        if let Some((dataset, snap_name)) = fork_origin.split_once('@') {
            match zfs::snapshot::destroy(dataset, snap_name) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("Warning: failed to clean up fork snapshot '{fork_origin}': {e}");
                }
            }
        }
        Ok(())
    }

    /// Mount a block device (zvol) at a temporary directory.
    ///
    /// Waits for the device to appear if needed (ZFS zvols may take a moment
    /// after creation). Returns the mount point path. The caller is
    /// responsible for calling [`unmount`] when done.
    fn mount(&self, path: &Path) -> Result<PathBuf> {
        // Wait for the device to appear (ZFS zvols created by clone may
        // not be immediately available).
        if !path.exists() {
            image::zvol::wait_for_device(path)?;
        }

        let mount_dir = tempfile::tempdir()
            .map_err(|e| Error::Io {
                path: std::env::temp_dir(),
                source: e,
            })?
            .keep();

        let output = ProcessCommand::new("mount")
            .arg(path)
            .arg(&mount_dir)
            .output()
            .map_err(|e| Error::CommandExec {
                command: "mount".to_string(),
                source: e,
            })?;

        if let Err(e) = Error::check_command("mount", output) {
            let _ = std::fs::remove_dir(&mount_dir);
            return Err(e);
        }

        Ok(mount_dir)
    }

    /// Unmount a filesystem and remove the mount point directory.
    fn unmount(&self, mount_point: &Path) -> Result<()> {
        super::image::umount(mount_point)?;
        let _ = std::fs::remove_dir(mount_point);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Check ext4 filesystem consistency before resize.
fn e2fsck(device: &Path) -> Result<()> {
    let output = ProcessCommand::new("e2fsck")
        .args(["-f", "-p"])
        .arg(device)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "e2fsck".to_string(),
            source: e,
        })?;

    // e2fsck exits 1 if it corrected errors (which -p does automatically).
    // Only treat exit >= 2 as failure.
    if output.status.code().unwrap_or(-1) >= 2 {
        return Err(Error::Command {
            command: "e2fsck".to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// Expand an ext4 filesystem to fill its block device.
fn resize2fs(device: &Path) -> Result<()> {
    let output = ProcessCommand::new("resize2fs")
        .arg(device)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "resize2fs".to_string(),
            source: e,
        })?;

    Error::check_command("resize2fs", output)?;
    Ok(())
}
