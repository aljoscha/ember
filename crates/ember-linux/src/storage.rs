//! Linux storage backend: ZFS zvols and clones.
//!
//! Wraps the `zfs::pool`, `zfs::dataset`, `zfs::volume`, and `zfs::snapshot`
//! modules behind the [`StorageBackend`] trait. On Linux, each VM's rootfs
//! is a ZFS zvol cloned from an image zvol's `@base` snapshot.
//!
//! The struct holds the ZFS dataset paths (derived from [`GlobalConfig`]) so
//! trait methods can construct full zvol paths from short names.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use crate::zfs;
use ember_core::backend::{
    GrowRequest, InitConfig, PoolUsage, StorageBackend, StorageUsage, VolumeHandle, VolumeUsage,
};
use ember_core::config::size::ByteSize;
use ember_core::config::GlobalConfig;
use ember_core::error::{Error, Result};
use ember_core::image::registry::ImageEntry;
use ember_core::state::vm::VmMetadata;

/// Linux storage backend using ZFS zvols.
#[derive(Clone)]
pub struct LinuxStorage {
    /// ZFS pool name (e.g., "tank"). Cached so `deinit` can call
    /// `zpool destroy` without re-reading the config.
    pool: String,
    /// Root of ember's dataset tree (e.g., "tank/ember").
    base_dataset: String,
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
            pool: config.pool.clone(),
            base_dataset: config.base_dataset(),
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
    fn create_image_volume(
        &self,
        name: &str,
        image_path: &Path,
        size_mib: u64,
    ) -> Result<VolumeHandle> {
        let zvol = self.image_zvol(name);

        // Create the zvol.
        zfs::volume::create(&zvol, size_mib)?;

        // Write the ext4 image to the zvol and create @base snapshot.
        // On failure, clean up the zvol.
        if let Err(e) = crate::zvol::write_to_zvol(image_path, &zvol) {
            let _ = zfs::volume::destroy(&zvol, true);
            return Err(e);
        }

        Ok(VolumeHandle::from_path(zvol))
    }

    /// Clone the image's `@base` snapshot to create a VM zvol.
    fn clone_for_vm(&self, image: &ImageEntry, vm_name: &str) -> Result<VolumeHandle> {
        let image_zvol = self.image_zvol(&image.local_name);
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
        Ok(VolumeHandle::from_path(vm_zvol))
    }

    /// Grow the zvol and expand the ext4 filesystem.
    fn resize(&self, vm: &VmMetadata, new_size: ByteSize) -> Result<()> {
        let zvol = self.vm_zvol(&vm.name);
        let new_gib = new_size
            .to_gib()
            .map_err(|e| Error::Zfs(format!("invalid resize target: {e}")))?;

        zfs::volume::set_volsize(&zvol, new_gib)?;

        // Wait for the device node to settle after resize, then expand ext4.
        let dev_path = zfs::volume::device_path(&zvol);
        crate::zvol::wait_for_device(&dev_path)?;
        e2fsck(&dev_path)?;
        resize2fs(&dev_path)?;

        Ok(())
    }

    /// Destroy the VM's zvol (and any internal fork snapshots beneath it).
    fn destroy_vm_storage(&self, vm: &VmMetadata) -> Result<()> {
        let zvol = self.vm_zvol(&vm.name);
        // Ignore errors — the zvol may already be gone.
        let _ = zfs::volume::destroy(&zvol, true);
        Ok(())
    }

    fn deinit(&self, _purge: bool) -> Result<()> {
        // `zpool destroy` is destructive — there is no equivalent of
        // "purge: keep the data". The flag is accepted for trait
        // uniformity but ignored here: ZFS pools always go.
        if !zfs::pool::exists(&self.pool)? {
            return Ok(());
        }
        let output = ProcessCommand::new("zpool")
            .args(["destroy", "-f", &self.pool])
            .output()
            .map_err(|e| Error::CommandExec {
                command: "zpool destroy".to_string(),
                source: e,
            })?;
        Error::check_command("zpool destroy", output)?;
        println!("Destroyed ZFS pool '{}'.", self.pool);
        Ok(())
    }

    fn grow(&self, _request: GrowRequest) -> Result<()> {
        Err(Error::Zfs(
            "ZFS pools auto-expand by default; use `zpool online -e` if needed".to_string(),
        ))
    }

    /// Destroy the image zvol (includes its @base snapshot).
    ///
    /// With `force: true`, uses `zfs destroy -R` to also destroy any orphaned
    /// dependent clones (VM zvols) that the application layer couldn't clean up.
    fn destroy_image_storage(&self, image: &ImageEntry, force: bool) -> Result<()> {
        let zvol = self.image_zvol(&image.local_name);
        if force {
            zfs::destroy_with_dependents(&zvol)
        } else {
            zfs::volume::destroy(&zvol, true)
        }
    }

    /// Device path for a VM's root disk zvol.
    fn disk_device_path(&self, vm: &VmMetadata) -> Result<PathBuf> {
        let zvol = self.vm_zvol(&vm.name);
        Ok(zfs::volume::device_path(&zvol))
    }

    /// Fork a VM's disk by snapshotting the source and cloning into a new VM.
    fn clone_vm_storage(&self, source: &VmMetadata, target_vm: &str) -> Result<VolumeHandle> {
        let source_zvol = self.vm_zvol(&source.name);
        let target_zvol = self.vm_zvol(target_vm);
        let snap_name = format!("fork-{target_vm}");

        // Create the snapshot on the source VM.
        zfs::snapshot::create(&source_zvol, &snap_name)?;

        let fork_snap_full = format!("{source_zvol}@{snap_name}");

        // Clone the snapshot into the target VM's zvol.
        if let Err(e) = zfs::volume::clone(&fork_snap_full, &target_zvol) {
            // Clean up the snapshot on failure.
            let _ = zfs::snapshot::destroy(&source_zvol, &snap_name);
            return Err(e);
        }

        Ok(VolumeHandle::from_path(target_zvol))
    }

    /// Rename a VM's zvol via `zfs rename`. Any `@fork-*` snapshots
    /// (and clones taken from them) ride along — ZFS reparents
    /// clone-origin references during rename.
    fn rename_vm_storage(&self, vm: &VmMetadata, new_name: &str) -> Result<VolumeHandle> {
        let old_zvol = self.vm_zvol(&vm.name);
        let new_zvol = self.vm_zvol(new_name);
        zfs::volume::rename(&old_zvol, &new_zvol)?;
        Ok(VolumeHandle::from_path(new_zvol))
    }

    /// Rename an image zvol (including its `@base` snapshot) via
    /// `zfs rename`. VM zvols cloned from `@base` are reparented
    /// automatically.
    fn rename_image_storage(
        &self,
        image: &ImageEntry,
        new_local_name: &str,
    ) -> Result<VolumeHandle> {
        let old_zvol = self.image_zvol(&image.local_name);
        let new_zvol = self.image_zvol(new_local_name);
        zfs::volume::rename(&old_zvol, &new_zvol)?;
        Ok(VolumeHandle::from_path(new_zvol))
    }

    /// Clean up the fork snapshot on the parent VM.
    ///
    /// Reconstructs the snapshot name from the naming convention:
    /// `{pool}/vms/{parent_vm}@fork-{forked_vm}`.
    fn cleanup_fork(&self, parent: &VmMetadata, forked: &VmMetadata) -> Result<()> {
        let parent_zvol = self.vm_zvol(&parent.name);
        let snap_name = format!("fork-{}", forked.name);
        match zfs::snapshot::destroy(&parent_zvol, &snap_name) {
            Ok(()) => {}
            Err(e) => {
                eprintln!(
                    "Warning: failed to clean up fork snapshot '{parent_zvol}@{snap_name}': {e}"
                );
            }
        }
        Ok(())
    }

    /// Check for fork snapshots on this VM's ZFS dataset.
    fn storage_dependents(&self, vm: &VmMetadata) -> Result<Vec<String>> {
        let zvol = self.vm_zvol(&vm.name);
        let snapshots = zfs::snapshot::list(&zvol)?;

        Ok(snapshots
            .into_iter()
            .filter_map(|s| s.short_name.strip_prefix("fork-").map(String::from))
            .collect())
    }

    fn usage(&self, vms: &[VmMetadata], images: &[ImageEntry]) -> Result<StorageUsage> {
        let totals = zfs::usage::totals(&self.base_dataset)?;
        let rows = zfs::usage::volumes(&self.base_dataset)?;
        let by_name: HashMap<&str, &zfs::usage::VolumeRow> =
            rows.iter().map(|r| (r.name.as_str(), r)).collect();

        // `logicalused` covers data only, while `used` also carries
        // every zvol's refreservation. Summing the reservations here
        // lets the pool ratio compare like with like. There is no
        // aggregate ZFS property for this, so it comes from the rows.
        let reserved: u64 = rows.iter().map(|r| r.used_by_refreservation).sum();

        // `disk_path` holds the dataset name on the ZFS backend, but a
        // record written by a different backend (or a half-created VM)
        // may not resolve. Those are dropped rather than zeroed.
        let volume_usage = |dataset: &str| -> Option<VolumeUsage> {
            let row = by_name.get(dataset)?;
            Some(VolumeUsage {
                provisioned: row.volsize,
                exclusive: row.used_by_dataset,
                referenced: Some(row.referenced),
                logical: Some(row.logical_referenced),
            })
        };

        Ok(StorageUsage {
            pool: PoolUsage {
                capacity: totals.used.saturating_add(totals.available),
                allocated: totals.used,
                reserved,
                logical: Some(totals.logical_used),
                // ZFS grows datasets against a shared pool rather than
                // promising space it does not have.
                addressable: None,
                metadata: None,
            },
            vms: vms
                .iter()
                .filter_map(|vm| Some((vm.name.clone(), volume_usage(&vm.disk_path)?)))
                .collect(),
            images: images
                .iter()
                .filter_map(|img| Some((img.local_name.clone(), volume_usage(&img.disk_path)?)))
                .collect(),
        })
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
            crate::zvol::wait_for_device(path)?;
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
        crate::image::umount(mount_point)?;
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
