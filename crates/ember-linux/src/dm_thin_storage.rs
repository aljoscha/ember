//! Linux storage backend using device-mapper thin provisioning.
//!
//! Replaces ZFS zvols with thin volumes from a dm-thin pool. The single
//! pool holds backing metadata + data devices (typically loopback files
//! under [`storage_path`](DmThinStorage::storage_path)) and exposes
//! arbitrary numbers of thin volumes as `/dev/mapper/ember-img-<name>`
//! and `/dev/mapper/ember-vm-<name>` block devices.
//!
//! See `docs/DM-THIN-SPEC.md` for the design.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use ember_core::backend::{InitConfig, SnapshotInfo, StorageBackend, VolumeHandle};
use ember_core::config::size::ByteSize;
use ember_core::config::{DmThinMode, GlobalConfig};
use ember_core::error::{Error, Result};
use ember_core::image::registry::ImageEntry;
use ember_core::state::vm::{SnapshotEntry, VmMetadata};

use crate::dm_thin::{loop_device, pool, thin, tools, SECTOR_SIZE};
use crate::zvol;

/// Default file name for the metadata backing file inside the dm-thin
/// data directory.
const METADATA_FILE: &str = "metadata.img";
/// Default file name for the data backing file inside the dm-thin
/// data directory.
const DATA_FILE: &str = "data.img";
/// Maximum thin volumes the metadata sizing assumes. dm-thin's
/// `thin_metadata_size` tool requires this; 1024 is a generous floor.
const DEFAULT_MAX_THINS: u64 = 1024;
/// Floor on metadata device size (32 MiB). The kernel rejects very
/// small metadata devices and `thin_metadata_size` may suggest values
/// below this for tiny pools.
const MIN_METADATA_SIZE_BYTES: u64 = 32 * 1024 * 1024;
/// Hard cap on metadata device size (16 GiB). The kernel won't accept
/// metadata devices larger than this.
const MAX_METADATA_SIZE_BYTES: u64 = 16 * 1024 * 1024 * 1024;

/// dm-thin storage backend.
///
/// Holds the configured backing path and pool block size; thin id state
/// lives on `VmMetadata`/`ImageEntry`/`SnapshotEntry`. Concurrent
/// invocations are race-free thanks to the kernel's atomic id rejection
/// in `create_thin`/`create_snap`.
#[derive(Clone)]
pub struct DmThinStorage {
    /// Backing path. Either a directory holding `metadata.img` and
    /// `data.img`, or a raw block device (the metadata file then lives
    /// under `<state_dir>/dm-thin-metadata.img`).
    storage_path: PathBuf,
    /// State directory (e.g. `/var/lib/ember`). Used as the persistent
    /// home for the metadata sparse file when `storage_path` points at
    /// a raw block device — `/dev/` is tmpfs on most distros and would
    /// lose the metadata across reboots.
    state_dir: PathBuf,
    /// Layout resolved at `ember init`. Pinning this rather than
    /// re-probing `storage_path.is_dir()` at runtime keeps reactivation
    /// deterministic if the filesystem disagrees with init (e.g., the
    /// directory was removed, or a raw device replaced a file).
    mode: DmThinMode,
    /// Pool block size in 512-byte sectors. Permanent at pool creation;
    /// the value here must match what the running pool was created with.
    block_size_sectors: u32,
}

impl DmThinStorage {
    /// Build the backend handle from a parsed [`GlobalConfig`].
    ///
    /// Falls back to [`pool::DEFAULT_BLOCK_SIZE_SECTORS`] when the
    /// config does not pin a block size, and to a live `is_dir()` probe
    /// when no [`DmThinMode`] is persisted (legacy configs predating
    /// the explicit field).
    pub fn new(config: &GlobalConfig) -> Self {
        let storage_path = config
            .storage_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("/var/lib/ember/dm-thin"));
        let mode = config.dm_thin_mode.unwrap_or_else(|| {
            if storage_path.is_dir() || !storage_path.exists() {
                DmThinMode::File
            } else {
                DmThinMode::RawDevice
            }
        });
        Self {
            storage_path,
            state_dir: config.state_dir.clone(),
            mode,
            block_size_sectors: config
                .dm_thin_block_size
                .unwrap_or(pool::DEFAULT_BLOCK_SIZE_SECTORS),
        }
    }

    /// Resolved metadata device path for the configured backing.
    fn metadata_file(&self) -> PathBuf {
        match self.mode {
            DmThinMode::File => self.storage_path.join(METADATA_FILE),
            // Raw block device: store metadata in the state directory
            // rather than next to the device. `/dev/` is tmpfs on most
            // distros and would vanish on reboot.
            DmThinMode::RawDevice => self.state_dir.join("dm-thin-metadata.img"),
        }
    }

    /// Resolved data device path for the configured backing.
    fn data_file(&self) -> PathBuf {
        match self.mode {
            DmThinMode::File => self.storage_path.join(DATA_FILE),
            DmThinMode::RawDevice => self.storage_path.clone(),
        }
    }

    /// Make sure the thin-pool device is active. Re-attaches loop
    /// devices and re-runs `dmsetup create` if the kernel state is gone
    /// (e.g., after a reboot).
    fn ensure_pool_active(&self) -> Result<()> {
        if pool::exists(pool::POOL_NAME)? {
            return Ok(());
        }

        pool::ensure_target_loaded()?;

        let metadata_path = self.metadata_file();
        let data_path = self.data_file();

        let metadata_loop = ensure_loop(&metadata_path)?;
        let data_loop = ensure_loop_or_block(&data_path)?;

        // Sanity-check metadata before activating; refuse to import a
        // dirty pool rather than risk corruption.
        if let Err(e) = tools::check(&metadata_loop) {
            return Err(Error::Command {
                command: "thin_check".to_string(),
                exit_code: 1,
                stderr: format!(
                    "metadata device {} failed thin_check; run thin_repair manually: {e}",
                    metadata_loop.display()
                ),
            });
        }

        let data_sectors = device_sectors(&data_loop)?;
        pool::create(
            pool::POOL_NAME,
            &metadata_loop,
            &data_loop,
            data_sectors,
            self.block_size_sectors,
            pool::DEFAULT_LOW_WATER_BLOCKS,
        )
    }

    /// Activate a thin volume if it is not already exposed under
    /// `/dev/mapper/<name>`.
    fn ensure_thin_active(
        &self,
        dm_name: &str,
        thin_id: u64,
        size_sectors: u64,
    ) -> Result<PathBuf> {
        if pool::exists(dm_name)? {
            return Ok(thin::device_path(dm_name));
        }
        thin::activate(dm_name, pool::POOL_NAME, thin_id, size_sectors)
    }

    /// Read a VM's required size in sectors from its metadata.
    fn vm_size_sectors(vm: &VmMetadata) -> u64 {
        let bytes = (vm.disk_size_gib as u64) * 1024 * 1024 * 1024;
        bytes / SECTOR_SIZE
    }

    /// Read a thin id off [`VmMetadata`] or fail with a clear message.
    fn require_vm_thin_id(vm: &VmMetadata) -> Result<u64> {
        vm.thin_id.ok_or_else(|| {
            Error::Vm(format!(
                "vm '{}' has no dm-thin id recorded — was the pool re-initialized?",
                vm.name
            ))
        })
    }

    /// Read a thin id off [`ImageEntry`] or fail with a clear message.
    fn require_image_thin_id(image: &ImageEntry) -> Result<u64> {
        image.thin_id.ok_or_else(|| {
            Error::Image(format!(
                "image '{}' has no dm-thin id recorded — was the pool re-initialized?",
                image.local_name
            ))
        })
    }
}

impl StorageBackend for DmThinStorage {
    fn init(config: &InitConfig) -> Result<()> {
        let storage_path = config.storage_path.clone().ok_or_else(|| {
            Error::Config("dm-thin requires --storage-path (directory or block device)".to_string())
        })?;

        pool::ensure_target_loaded()?;

        let block_size_sectors = config
            .dm_thin_block_size
            .unwrap_or(pool::DEFAULT_BLOCK_SIZE_SECTORS);

        // Layout (file vs raw device) is resolved by the CLI — the
        // backend trusts what it was handed instead of re-probing the
        // filesystem.
        let mode = config.dm_thin_mode.ok_or_else(|| {
            Error::Config("dm-thin requires a resolved layout mode in InitConfig".to_string())
        })?;

        // Resolve metadata + data file paths and create them as sparse
        // files when missing. A raw block device is kept as-is for the
        // data side.
        let (metadata_path, data_path) = resolve_init_paths(&storage_path, &config.state_dir, mode);

        let pool_size_bytes = match config.dm_thin_size {
            Some(size) => size.bytes(),
            None => match mode {
                DmThinMode::RawDevice => device_size_bytes(&data_path)?,
                DmThinMode::File => {
                    return Err(Error::Config(
                        "dm-thin --size is required when using a file-backed pool".to_string(),
                    ));
                }
            },
        };

        // Compute metadata size (or use an explicit override).
        let metadata_size_bytes = match config.dm_thin_metadata_size {
            Some(size) => size.bytes(),
            None => {
                let block_size_bytes = (block_size_sectors as u64) * SECTOR_SIZE;
                let recommended =
                    tools::metadata_size(pool_size_bytes, block_size_bytes, DEFAULT_MAX_THINS)?;
                recommended.clamp(MIN_METADATA_SIZE_BYTES, MAX_METADATA_SIZE_BYTES)
            }
        };

        // Create sparse files when the user supplied paths that don't
        // yet exist. A raw block device is left alone here.
        if metadata_path.extension().is_some() && !metadata_path.exists() {
            ensure_parent_dir(&metadata_path)?;
            create_sparse_file(&metadata_path, metadata_size_bytes)?;
        }
        if data_path.is_file() || !data_path.exists() {
            ensure_parent_dir(&data_path)?;
            if !data_path.exists() {
                create_sparse_file(&data_path, pool_size_bytes)?;
            }
        }

        // Zero the first 4 KiB of the metadata device — the kernel uses
        // an all-zero superblock as the signal to format a fresh pool.
        zero_head(&metadata_path)?;

        // Attach loops, then assemble the pool. If anything past this
        // point fails, detach the loops we attached so we don't leak
        // them pointing at backing files that may get cleaned up.
        let metadata_loop = ensure_loop(&metadata_path)?;
        let data_loop = match ensure_loop_or_block(&data_path) {
            Ok(p) => p,
            Err(e) => {
                let _ = loop_device::detach(&metadata_loop);
                return Err(e);
            }
        };

        let data_sectors = match device_sectors(&data_loop) {
            Ok(s) => s,
            Err(e) => {
                let _ = loop_device::detach(&metadata_loop);
                if data_path.is_file() {
                    let _ = loop_device::detach(&data_loop);
                }
                return Err(e);
            }
        };
        if let Err(e) = pool::create(
            pool::POOL_NAME,
            &metadata_loop,
            &data_loop,
            data_sectors,
            block_size_sectors,
            pool::DEFAULT_LOW_WATER_BLOCKS,
        ) {
            let _ = loop_device::detach(&metadata_loop);
            if data_path.is_file() {
                let _ = loop_device::detach(&data_loop);
            }
            return Err(e);
        }

        println!(
            "dm-thin pool '{}' active ({} data, {} block size).",
            pool::POOL_NAME,
            format_bytes(pool_size_bytes),
            format_bytes((block_size_sectors as u64) * SECTOR_SIZE),
        );

        Ok(())
    }

    fn create_image_volume(
        &self,
        name: &str,
        image_path: &Path,
        size_mib: u64,
    ) -> Result<VolumeHandle> {
        self.ensure_pool_active()?;

        let staging_dm = thin::image_staging_dm_name(name);
        let final_dm = thin::image_dm_name(name);
        let size_sectors = (size_mib * 1024 * 1024) / SECTOR_SIZE;

        // 1. Allocate a fresh staging thin and write the ext4 image.
        let staging_id = thin::allocate(pool::POOL_NAME)?;
        let staging_dev =
            match thin::activate(&staging_dm, pool::POOL_NAME, staging_id, size_sectors) {
                Ok(p) => p,
                Err(e) => {
                    let _ = thin::delete(pool::POOL_NAME, staging_id);
                    return Err(e);
                }
            };

        // 2. dd the ext4 image onto the staging device.
        if let Err(e) = dd_image(image_path, &staging_dev) {
            let _ = thin::deactivate(&staging_dm);
            let _ = thin::delete(pool::POOL_NAME, staging_id);
            return Err(e);
        }

        // 3. Snapshot the staging volume as the immutable base. Suspend
        //    the staging device first so the snapshot sees a coherent
        //    metadata commit; resume it on the way out either way.
        let base_id_result = thin::suspend(&staging_dm).and_then(|()| {
            let id = thin::allocate_snap(pool::POOL_NAME, staging_id);
            let _ = thin::resume(&staging_dm);
            id
        });
        let base_id = match base_id_result {
            Ok(id) => id,
            Err(e) => {
                let _ = thin::deactivate(&staging_dm);
                let _ = thin::delete(pool::POOL_NAME, staging_id);
                return Err(e);
            }
        };

        // 4. Drop the staging device + thin id; the base id retains all
        //    of its blocks.
        let _ = thin::deactivate(&staging_dm);
        let _ = thin::delete(pool::POOL_NAME, staging_id);

        // The base thin is left inactive. Lazy activation creates the
        // device on first use. Record the would-be path so it can be
        // displayed and so callers see a stable identifier.
        Ok(VolumeHandle {
            disk_path: thin::device_path(&final_dm),
            thin_id: Some(base_id),
        })
    }

    fn clone_for_vm(&self, image: &ImageEntry, vm_name: &str) -> Result<VolumeHandle> {
        self.ensure_pool_active()?;
        let base_id = Self::require_image_thin_id(image)?;

        let dm_name = thin::vm_dm_name(vm_name);
        // The VM's virtual size matches the image's size at clone time;
        // resize to a larger disk happens in a subsequent `resize` call.
        let size_sectors = (image.size_mib * 1024 * 1024) / SECTOR_SIZE;

        let vm_id = thin::allocate_snap(pool::POOL_NAME, base_id)?;
        match thin::activate(&dm_name, pool::POOL_NAME, vm_id, size_sectors) {
            Ok(disk_path) => Ok(VolumeHandle {
                disk_path,
                thin_id: Some(vm_id),
            }),
            Err(e) => {
                let _ = thin::delete(pool::POOL_NAME, vm_id);
                Err(e)
            }
        }
    }

    fn snapshot(&self, vm: &VmMetadata, snap_name: &str) -> Result<Option<SnapshotEntry>> {
        self.ensure_pool_active()?;
        let vm_id = Self::require_vm_thin_id(vm)?;
        let dm_name = thin::vm_dm_name(&vm.name);
        let size_sectors = Self::vm_size_sectors(vm);

        // Suspend so create_snap sees a metadata-coherent volume.
        // Some operations (e.g. snapshotting a never-activated volume)
        // can run without an active device, but suspending an inactive
        // device errors. Activate first if needed.
        self.ensure_thin_active(&dm_name, vm_id, size_sectors)?;

        thin::suspend(&dm_name)?;
        let snap_result = thin::allocate_snap(pool::POOL_NAME, vm_id);
        let _ = thin::resume(&dm_name);
        let snap_id = snap_result?;

        Ok(Some(SnapshotEntry {
            name: snap_name.to_string(),
            thin_id: snap_id,
            created_at: ember_core::state::vm::now_epoch_secs(),
            size_sectors,
        }))
    }

    fn restore_snapshot(&self, vm: &VmMetadata, snap_name: &str) -> Result<VolumeHandle> {
        self.ensure_pool_active()?;
        let vm_id = Self::require_vm_thin_id(vm)?;
        let snap = vm
            .snapshots
            .iter()
            .find(|s| s.name == snap_name)
            .ok_or_else(|| {
                Error::Vm(format!(
                    "snapshot '{snap_name}' not found on vm '{}'",
                    vm.name
                ))
            })?;
        let snap_id = snap.thin_id;

        let dm_name = thin::vm_dm_name(&vm.name);
        let size_sectors = Self::vm_size_sectors(vm);

        // Allocate the replacement thin id from the snapshot up-front so
        // a failure here leaves `vm.thin_id` and the kernel pool
        // unchanged. The old order (deactivate -> delete -> allocate)
        // would orphan `vm.thin_id` on any allocate hiccup.
        let new_id = thin::allocate_snap(pool::POOL_NAME, snap_id)?;

        // Once new_id exists, swap the dm-mapper slot over to it. Any
        // failure from here on must release new_id so we don't leak
        // kernel state.
        let result = (|| -> Result<PathBuf> {
            if pool::exists(&dm_name)? {
                thin::deactivate(&dm_name)?;
            }
            thin::delete(pool::POOL_NAME, vm_id)?;
            thin::activate(&dm_name, pool::POOL_NAME, new_id, size_sectors)
        })();

        match result {
            Ok(disk_path) => Ok(VolumeHandle {
                disk_path,
                thin_id: Some(new_id),
            }),
            Err(e) => {
                let _ = thin::delete(pool::POOL_NAME, new_id);
                Err(e)
            }
        }
    }

    fn delete_snapshot(&self, vm: &VmMetadata, snap_name: &str) -> Result<()> {
        self.ensure_pool_active()?;
        let snap = vm
            .snapshots
            .iter()
            .find(|s| s.name == snap_name)
            .ok_or_else(|| {
                Error::Vm(format!(
                    "snapshot '{snap_name}' not found on vm '{}'",
                    vm.name
                ))
            })?;
        thin::delete(pool::POOL_NAME, snap.thin_id)
    }

    fn list_snapshots(&self, vm: &VmMetadata) -> Result<Vec<SnapshotInfo>> {
        // dm-thin tracks snapshots via the persisted `vm.snapshots`
        // list; the kernel knows nothing about names.
        Ok(vm
            .snapshots
            .iter()
            .map(|s| SnapshotInfo {
                name: s.name.clone(),
                created_at: s.created_at,
                size: s.size_sectors * SECTOR_SIZE,
            })
            .collect())
    }

    fn resize(&self, vm: &VmMetadata, new_size: ByteSize) -> Result<()> {
        self.ensure_pool_active()?;
        let vm_id = Self::require_vm_thin_id(vm)?;
        let dm_name = thin::vm_dm_name(&vm.name);
        let new_sectors = new_size.bytes() / SECTOR_SIZE;

        // Activate (lazy) so we have a device to reload.
        let current_sectors = Self::vm_size_sectors(vm);
        let dev_path = self.ensure_thin_active(&dm_name, vm_id, current_sectors)?;

        thin::reload_size(&dm_name, pool::POOL_NAME, vm_id, new_sectors)?;
        zvol::wait_for_device(&dev_path)?;
        e2fsck(&dev_path)?;
        resize2fs(&dev_path)?;
        Ok(())
    }

    fn destroy_vm_storage(&self, vm: &VmMetadata) -> Result<()> {
        // Best-effort: deactivate first, then free the thin id. Either
        // step may already be done by an earlier failure path.
        let _ = self.ensure_pool_active();
        let dm_name = thin::vm_dm_name(&vm.name);
        if let Ok(true) = pool::exists(&dm_name) {
            let _ = thin::deactivate(&dm_name);
        }
        if let Some(id) = vm.thin_id {
            let _ = thin::delete(pool::POOL_NAME, id);
        }
        Ok(())
    }

    fn destroy_image_storage(&self, image: &ImageEntry, _force: bool) -> Result<()> {
        // dm-thin reference-counts blocks; deleting the base thin is
        // safe even when VMs still have clones — they keep their own
        // thin ids and stay readable. `force` doesn't change behavior.
        let _ = self.ensure_pool_active();
        let dm_name = thin::image_dm_name(&image.local_name);
        if let Ok(true) = pool::exists(&dm_name) {
            let _ = thin::deactivate(&dm_name);
        }
        if let Some(id) = image.thin_id {
            let _ = thin::delete(pool::POOL_NAME, id);
        }
        Ok(())
    }

    fn disk_device_path(&self, vm: &VmMetadata) -> Result<PathBuf> {
        // Ensure the pool table and the per-VM thin device are live in
        // the kernel. After a host reboot both are gone; without this,
        // `vm start` would hand Firecracker a stale `/dev/mapper/...`
        // path that resolves to ENOENT.
        self.ensure_pool_active()?;
        let thin_id = Self::require_vm_thin_id(vm)?;
        let dm_name = thin::vm_dm_name(&vm.name);
        let size_sectors = Self::vm_size_sectors(vm);
        self.ensure_thin_active(&dm_name, thin_id, size_sectors)
    }

    fn clone_vm_storage(&self, source: &VmMetadata, target_vm: &str) -> Result<VolumeHandle> {
        self.ensure_pool_active()?;
        let source_id = Self::require_vm_thin_id(source)?;
        let dm_name = thin::vm_dm_name(target_vm);
        let size_sectors = Self::vm_size_sectors(source);

        let fork_id = thin::allocate_snap(pool::POOL_NAME, source_id)?;
        match thin::activate(&dm_name, pool::POOL_NAME, fork_id, size_sectors) {
            Ok(disk_path) => Ok(VolumeHandle {
                disk_path,
                thin_id: Some(fork_id),
            }),
            Err(e) => {
                let _ = thin::delete(pool::POOL_NAME, fork_id);
                Err(e)
            }
        }
    }

    fn cleanup_fork(&self, _parent: &VmMetadata, _forked: &VmMetadata) -> Result<()> {
        // dm-thin forks are independent — the snapshot id used to
        // create the fork is the fork's own thin id, not a marker on
        // the parent. Nothing to clean up on the parent.
        Ok(())
    }

    fn storage_dependents(&self, _vm: &VmMetadata) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn deinit(&self, purge: bool) -> Result<()> {
        // 1. Deactivate every ember-managed thin volume so the pool
        //    can be removed cleanly.
        for prefix in [thin::IMAGE_PREFIX, thin::VM_PREFIX] {
            for name in pool::list_with_prefix(prefix)? {
                let _ = thin::deactivate(&name);
            }
        }
        // 2. Drop the pool itself (if active).
        if pool::exists(pool::POOL_NAME)? {
            pool::remove(pool::POOL_NAME)?;
        }
        // 3. Detach the loop devices, if any.
        let metadata_path = self.metadata_file();
        let data_path = self.data_file();
        if let Some(loop_dev) = loop_device::find_for(&metadata_path)? {
            let _ = loop_device::detach(&loop_dev);
        }
        if let Some(loop_dev) = loop_device::find_for(&data_path)? {
            let _ = loop_device::detach(&loop_dev);
        }
        // 4. Optionally delete the backing files. A raw block device
        //    supplied by the user is always left alone.
        if purge {
            for path in [&metadata_path, &data_path] {
                if path.is_file() {
                    let _ = fs::remove_file(path);
                }
            }
            // Remove the dm-thin directory itself if empty.
            if self.storage_path.is_dir() {
                let _ = fs::remove_dir(&self.storage_path);
            }
        }
        println!("dm-thin pool '{}' torn down.", pool::POOL_NAME);
        Ok(())
    }

    fn grow(&self, new_size: ByteSize) -> Result<()> {
        self.ensure_pool_active()?;

        let data_path = self.data_file();
        let new_bytes = new_size.bytes();

        if data_path.is_file() {
            create_sparse_file(&data_path, new_bytes)?;
        } else {
            return Err(Error::Config(format!(
                "data device {} is a raw block device — grow it externally first \
                 (e.g. lvextend, cloud-volume resize) and then re-run `ember storage grow`",
                data_path.display()
            )));
        }

        // Make the loop driver pick up the new file size, then reload
        // the pool table with the larger sector count.
        let metadata_path = self.metadata_file();
        let metadata_loop = loop_device::find_for(&metadata_path)?.ok_or_else(|| {
            Error::Config(format!(
                "metadata device {} is not attached to a loop device",
                metadata_path.display()
            ))
        })?;
        let data_loop = if data_path.is_file() {
            let dev = loop_device::find_for(&data_path)?.ok_or_else(|| {
                Error::Config(format!(
                    "data device {} is not attached to a loop device",
                    data_path.display()
                ))
            })?;
            loop_device::refresh_size(&dev)?;
            dev
        } else {
            data_path.clone()
        };

        let data_sectors = device_sectors(&data_loop)?;
        pool::reload(
            pool::POOL_NAME,
            &metadata_loop,
            &data_loop,
            data_sectors,
            self.block_size_sectors,
            pool::DEFAULT_LOW_WATER_BLOCKS,
        )?;
        println!(
            "Grew dm-thin pool data device to {}.",
            format_bytes(new_bytes)
        );
        Ok(())
    }

    fn mount(&self, path: &Path) -> Result<PathBuf> {
        zvol::wait_for_device(path)?;

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
            let _ = fs::remove_dir(&mount_dir);
            return Err(e);
        }
        Ok(mount_dir)
    }

    fn unmount(&self, mount_point: &Path) -> Result<()> {
        crate::image::umount(mount_point)?;
        let _ = fs::remove_dir(mount_point);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decide where the metadata + data backing live based on the
/// caller-resolved [`DmThinMode`].
///
/// * [`DmThinMode::File`]: `metadata.img`/`data.img` inside `storage_path`.
/// * [`DmThinMode::RawDevice`]: `storage_path` is the data device, with
///   metadata as a sparse file under `state_dir` (a raw device's parent
///   is `/dev/`, which is tmpfs and would lose the metadata on reboot).
fn resolve_init_paths(
    storage_path: &Path,
    state_dir: &Path,
    mode: DmThinMode,
) -> (PathBuf, PathBuf) {
    match mode {
        DmThinMode::File => (
            storage_path.join(METADATA_FILE),
            storage_path.join(DATA_FILE),
        ),
        DmThinMode::RawDevice => (
            state_dir.join("dm-thin-metadata.img"),
            storage_path.to_path_buf(),
        ),
    }
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    Ok(())
}

/// Create a sparse file of the given byte size using `truncate`.
fn create_sparse_file(path: &Path, size_bytes: u64) -> Result<()> {
    let output = ProcessCommand::new("truncate")
        .args(["-s", &size_bytes.to_string()])
        .arg(path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "truncate".to_string(),
            source: e,
        })?;
    Error::check_command("truncate", output)?;
    Ok(())
}

/// Zero the first 4 KiB of a file or block device. dm-thin uses an
/// all-zero superblock as its "format me" sentinel.
fn zero_head(path: &Path) -> Result<()> {
    let output = ProcessCommand::new("dd")
        .arg("if=/dev/zero")
        .arg(format!("of={}", path.display()))
        .args(["bs=4K", "count=1", "conv=notrunc", "status=none"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dd zero metadata".to_string(),
            source: e,
        })?;
    Error::check_command("dd zero metadata", output)?;
    Ok(())
}

/// Find an existing loop device for `file`, or attach a new one.
fn ensure_loop(file: &Path) -> Result<PathBuf> {
    if let Some(existing) = loop_device::find_for(file)? {
        return Ok(existing);
    }
    loop_device::attach(file)
}

/// Same as [`ensure_loop`] but transparent for raw block devices: if
/// the path is a block device (not a regular file) it's used as-is.
fn ensure_loop_or_block(path: &Path) -> Result<PathBuf> {
    let metadata = fs::metadata(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    if metadata.file_type().is_file() {
        ensure_loop(path)
    } else {
        Ok(path.to_path_buf())
    }
}

/// Number of 512-byte sectors on a block device.
fn device_sectors(path: &Path) -> Result<u64> {
    Ok(device_size_bytes(path)? / SECTOR_SIZE)
}

/// Total byte size of a block device (or regular file). Wraps
/// `blockdev --getsize64` for block devices and falls back to file
/// metadata otherwise.
fn device_size_bytes(path: &Path) -> Result<u64> {
    if let Ok(meta) = fs::metadata(path) {
        if meta.file_type().is_file() {
            return Ok(meta.len());
        }
    }
    let output = ProcessCommand::new("blockdev")
        .arg("--getsize64")
        .arg(path)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "blockdev --getsize64".to_string(),
            source: e,
        })?;
    let output = Error::check_command("blockdev --getsize64", output)?;
    let s = String::from_utf8_lossy(&output.stdout);
    s.trim().parse::<u64>().map_err(|e| Error::Command {
        command: "blockdev --getsize64".to_string(),
        exit_code: 0,
        stderr: format!("non-numeric size {:?}: {e}", s.trim()),
    })
}

/// Format a byte count for log lines.
fn format_bytes(bytes: u64) -> String {
    const TIB: u64 = 1024 * 1024 * 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= TIB {
        format!("{:.1} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Run `dd` to copy an image file onto a block device.
fn dd_image(image_path: &Path, device: &Path) -> Result<()> {
    let output = ProcessCommand::new("dd")
        .arg(format!("if={}", image_path.display()))
        .arg(format!("of={}", device.display()))
        .args(["bs=1M", "conv=fsync", "status=none"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dd image to thin".to_string(),
            source: e,
        })?;
    Error::check_command("dd image to thin", output)?;
    Ok(())
}

/// `e2fsck -f -p` — used before resize2fs.
fn e2fsck(device: &Path) -> Result<()> {
    let output = ProcessCommand::new("e2fsck")
        .args(["-f", "-p"])
        .arg(device)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "e2fsck".to_string(),
            source: e,
        })?;
    if output.status.code().unwrap_or(-1) >= 2 {
        return Err(Error::Command {
            command: "e2fsck".to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(())
}

/// `resize2fs` — expand the ext4 filesystem to fill the device.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
        assert_eq!(format_bytes(3u64 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
