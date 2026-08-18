//! Linux storage backend using device-mapper thin provisioning.
//!
//! Replaces ZFS zvols with thin volumes from a dm-thin pool. The single
//! pool holds backing metadata + data devices (typically loopback files
//! under [`storage_path`](DmThinStorage::storage_path)) and exposes
//! arbitrary numbers of thin volumes as `/dev/mapper/ember-img-<name>`
//! and `/dev/mapper/ember-vm-<name>` block devices.
//!
//! See `docs/DM-THIN-SPEC.md` for the design.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use ember_core::backend::{
    GrowRequest, InitConfig, MetadataUsage, PoolUsage, StorageBackend, StorageUsage, VolumeHandle,
    VolumeUsage,
};
use ember_core::config::size::ByteSize;
use ember_core::config::{DmThinMode, GlobalConfig, VdoConfig};
use ember_core::error::{Error, Result};
use ember_core::image::registry::ImageEntry;
use ember_core::state::store::StateStore;
use ember_core::state::vm::VmMetadata;

use crate::dm::{self, SECTOR_SIZE};
use crate::dm_thin::{loop_device, pool, thin, tools};
use crate::vdo;
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
/// lives on `VmMetadata`/`ImageEntry`. Concurrent invocations are
/// race-free thanks to the kernel's atomic id rejection in
/// `create_thin`/`create_snap`.
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
    /// Per-installation device-mapper pool name, e.g.
    /// `ember-a3f4-pool`. Pinned from `GlobalConfig` at construction
    /// rather than recomputed at every call site so the backend acts on
    /// exactly the pool the persisted config refers to.
    pool_name: String,
    /// Per-installation prefix for image base volumes
    /// (`ember-a3f4-img-`).
    image_prefix: String,
    /// Per-installation prefix for VM disks (`ember-a3f4-vm-`).
    vm_prefix: String,
    /// dm-vdo compression layer beneath the pool's data device, when
    /// the installation was initialized with one. `None` means the
    /// backing device is the data device.
    vdo: Option<VdoConfig>,
    /// Per-installation VDO device name (`ember-a3f4-vdo`). Derived
    /// unconditionally so teardown can sweep for it even on a config
    /// that has no VDO layer recorded.
    vdo_name: String,
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
        // dm-thin owns its own name derivation; we just feed it the
        // install's namespace (or `None` for legacy configs).
        let ns = config.instance_namespace();
        Self {
            storage_path,
            state_dir: config.state_dir.clone(),
            mode,
            block_size_sectors: config
                .dm_thin_block_size
                .unwrap_or(pool::DEFAULT_BLOCK_SIZE_SECTORS),
            pool_name: pool::name(ns),
            image_prefix: thin::image_prefix(ns),
            vm_prefix: thin::vm_prefix(ns),
            vdo: config.vdo,
            vdo_name: vdo::name(ns),
        }
    }

    /// Table parameters for this installation's VDO volume.
    ///
    /// `max_discard_blocks` comes from the pool block size so that a
    /// single pool-block discard passes down to VDO as one bio rather
    /// than being split into sixteen by the kernel's default of one
    /// 4 KiB block.
    fn vdo_params(&self, config: VdoConfig) -> vdo::Params {
        vdo_params(config, self.block_size_sectors)
    }

    /// Record the VDO layer's sizes in `config.json`.
    ///
    /// The backend owns this rather than reporting the values upward,
    /// because only it knows the instant at which they become true: the
    /// kernel persists them inside the volume on a successful resume,
    /// and from that moment a config still describing the old sizes is
    /// a pool that will not activate.
    fn persist_vdo_config(&self, vdo: VdoConfig) -> Result<()> {
        let store = StateStore::new(self.state_dir.clone());
        store
            .update(&store.config_path(), |c: &mut GlobalConfig| {
                c.vdo = Some(vdo);
                Ok(())
            })
            .map_err(|e| {
                Error::Config(format!(
                    "the VDO volume was grown to {} physical / {} logical, but {} could \
                     not be updated: {e}. Set those two values under \"vdo\" by hand, \
                     or the pool will not activate again.",
                    vdo.physical_size,
                    vdo.logical_size,
                    store.config_path().display(),
                ))
            })
    }

    /// The block device the thin pool should use for data, bringing the
    /// VDO layer up first when the installation has one.
    ///
    /// Without VDO this is the loop device over `data.img` (or the raw
    /// block device). With VDO it is the VDO device, and the loop
    /// device underneath it becomes VDO's private backing store.
    fn ensure_data_device(&self) -> Result<PathBuf> {
        let backing = ensure_loop_or_block(&self.data_file())?;
        let Some(vdo_config) = self.vdo else {
            return Ok(backing);
        };
        if !dm::device_exists(&self.vdo_name)? {
            vdo::ensure_target_loaded()?;
            vdo::activate(&self.vdo_name, &backing, &self.vdo_params(vdo_config))?;
        }
        let path = vdo::device_path(&self.vdo_name);
        zvol::wait_for_device(&path)?;
        // A read-only volume is fatal wherever it is noticed, and this
        // is the only gate on the path `vm start` takes. Without it a
        // guest boots onto a disk whose every write returns EIO.
        vdo::assert_read_write(&self.vdo_name, &vdo::status(&self.vdo_name)?)?;
        Ok(path)
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
        if dm::device_exists(&self.pool_name)? {
            return Ok(());
        }

        pool::ensure_target_loaded()?;

        let metadata_loop = ensure_loop(&self.metadata_file())?;
        let data_dev = self.ensure_data_device()?;

        // Sanity-check metadata before activating; refuse to import a
        // dirty pool rather than risk corruption. The metadata device
        // never goes through VDO, so this reads the loop device
        // directly whether or not the data side is compressed.
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

        let data_sectors = device_sectors(&data_dev)?;
        pool::create(
            &self.pool_name,
            &metadata_loop,
            &data_dev,
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
        if dm::device_exists(dm_name)? {
            return Ok(thin::device_path(dm_name));
        }
        thin::activate(dm_name, &self.pool_name, thin_id, size_sectors)
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

    /// Refuse allocating-or-writing operations when the pool has gone
    /// read-only, run out of data, or failed entirely. Without this
    /// gate, callers see opaque `EIO` mid-`dd` (out of space) or
    /// silent thin id leaks on metadata-corrupt pools.
    ///
    /// `grow` is intentionally not gated because it is the recovery
    /// path for [`PoolMode::OutOfDataSpace`]; destroy paths are also
    /// not gated since freeing thin ids must work even on a sick pool.
    fn assert_pool_healthy(&self) -> Result<()> {
        // VDO first: when it is sick or full the pool is only the
        // messenger, and naming the pool would send the operator to the
        // wrong recovery tool.
        if self.vdo.is_some() {
            let status = vdo::status(&self.vdo_name)?;
            vdo::assert_healthy(&self.vdo_name, &status)?;
            for warning in vdo::warnings(&self.vdo_name, &status) {
                eprintln!("{warning}");
            }
        }
        let status = pool::status(&self.pool_name)?;
        match status.mode {
            pool::PoolMode::ReadWrite => Ok(()),
            pool::PoolMode::ReadOnly => Err(Error::Pool(format!(
                "dm-thin pool '{}' is read-only — run `thin_check` and `thin_repair` to recover",
                self.pool_name
            ))),
            pool::PoolMode::OutOfDataSpace => Err(Error::Pool(format!(
                "dm-thin pool '{}' is out of data space ({}/{} blocks used) — run `ember storage grow --size <bigger>` to extend it",
                self.pool_name,
                status.used_data_blocks,
                status.total_data_blocks,
            ))),
            pool::PoolMode::Failed => Err(Error::Pool(format!(
                "dm-thin pool '{}' has failed — inspect dmesg and `thin_check` the metadata device",
                self.pool_name
            ))),
        }
    }
}

impl StorageBackend for DmThinStorage {
    fn init(config: &InitConfig) -> Result<()> {
        let storage_path = config.storage_path.clone().ok_or_else(|| {
            Error::Config("dm-thin requires --storage-path (directory or block device)".to_string())
        })?;

        pool::ensure_target_loaded()?;

        // Pool is named per-installation so two installs on one host
        // don't share kernel state. `init` is only ever run on a
        // fresh install (the CLI always pins a real `instance_id`),
        // so feeding `pool::name` a `Some` here matches what
        // `DmThinStorage::new` derives from the persisted config.
        let pool_name = pool::name(Some(&config.instance_id));

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

        let pool_size_bytes = resolve_pool_size(&data_path, mode, config.dm_thin_size)?;

        // The CLI resolves the VDO sizes so `InitConfig` and the
        // persisted `GlobalConfig` cannot disagree, which means it has
        // already worked out the physical size independently. If the
        // two answers differ, the pool would be built at one size and
        // recorded at another, and every later activation would fail on
        // a bare EINVAL. Catch it here where it can still be explained.
        if let Some(vdo_config) = config.vdo {
            vdo::ensure_target_loaded()?;
            vdo::ensure_format_tool()?;
            vdo::check_physical_size(vdo_config.physical_size)?;
            // `vdoformat` takes no physical size: it formats the whole
            // device. So a recorded size smaller than the backing
            // device produces a table the kernel reads as a shrink and
            // rejects, on this and every later activation.
            //
            // The size that matters is what `vdoformat` will actually
            // see, which for a leftover `data.img` is its real length
            // rather than whatever `--size` asked for.
            let backing_size = if data_path.exists() {
                device_size_bytes(&data_path)?
            } else {
                pool_size_bytes
            };
            if vdo::align_down(backing_size) != vdo_config.physical_size {
                // Two very different situations reach this, with two
                // different fixes. A raw device is whatever size it is,
                // so `--size` has to match it. A file this run did not
                // create is a leftover from a `deinit` without
                // `--purge`, and matching `--size` to it only trades
                // this error for `vdoformat` refusing to overwrite a
                // volume it already holds.
                let fix = if data_path.is_file() {
                    format!(
                        "{} is a leftover pool from an earlier install, so delete it \
                         (or re-run `ember deinit --purge`) before initializing again",
                        data_path.display(),
                    )
                } else {
                    format!(
                        "pass --size {} to match {}, or point --storage-path at a \
                         device of the requested size",
                        format_bytes(vdo::align_down(backing_size)),
                        data_path.display(),
                    )
                };
                return Err(Error::Config(format!(
                    "--vdo uses the whole backing device, so the pool's physical size \
                     must match it: {} is {}, but {} was requested. {fix}.",
                    data_path.display(),
                    format_bytes(backing_size),
                    format_bytes(vdo_config.physical_size),
                )));
            }
        }

        // Metadata is sized for the space the pool can *address*, which
        // is not the same as the disk it sits on once a compression
        // layer lets it hand out more than it has. Sizing from the
        // physical figure would leave an over-provisioned pool running
        // out of metadata at the fraction of its capacity the two sizes
        // differ by, and metadata exhaustion drops it to read-only.
        let addressable_bytes = config
            .vdo
            .map_or(pool_size_bytes, |vdo| vdo.logical_size.max(pool_size_bytes));
        let metadata_size_bytes = match config.dm_thin_metadata_size {
            Some(size) => size.bytes(),
            None => {
                let block_size_bytes = (block_size_sectors as u64) * SECTOR_SIZE;
                let recommended =
                    tools::metadata_size(addressable_bytes, block_size_bytes, DEFAULT_MAX_THINS)?;
                recommended.clamp(MIN_METADATA_SIZE_BYTES, MAX_METADATA_SIZE_BYTES)
            }
        };

        // Create sparse files when the user supplied paths that don't
        // yet exist. A raw block device is left alone here.
        if metadata_path.extension().is_some() && !metadata_path.exists() {
            ensure_parent_dir(&metadata_path)?;
            create_sparse_file(&metadata_path, metadata_size_bytes)?;
        }
        // Track whether this run created the data file. If the stack
        // fails to come up we delete it again, because `vdoformat`
        // refuses a device that already holds a volume and would
        // otherwise block every retry with no hint as to why.
        let mut created_data_file = false;
        if data_path.is_file() || !data_path.exists() {
            ensure_parent_dir(&data_path)?;
            if !data_path.exists() {
                create_sparse_file(&data_path, pool_size_bytes)?;
                created_data_file = true;
            }
        }

        // Zero the first 4 KiB of the metadata device — the kernel uses
        // an all-zero superblock as the signal to format a fresh pool.
        zero_head(&metadata_path)?;

        // Attach the loops and assemble the stack. Anything that fails
        // past this point is undone: a half-built pool holding loop
        // devices open against backing files is worse than none.
        let metadata_loop = ensure_loop(&metadata_path)?;
        let vdo_name = vdo::name(Some(&config.instance_id));
        if let Err(e) = assemble_stack(
            &pool_name,
            &vdo_name,
            config.vdo,
            &metadata_loop,
            &data_path,
            block_size_sectors,
        ) {
            unwind_stack(&vdo_name, &metadata_loop, &data_path, created_data_file);
            return Err(e);
        }

        println!(
            "dm-thin pool '{pool_name}' active ({} data capacity, {} block size).",
            format_bytes(addressable_bytes),
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
        self.assert_pool_healthy()?;

        let staging_dm = thin::image_staging_dm_name(&self.image_prefix, name);
        let final_dm = thin::image_dm_name(&self.image_prefix, name);
        let size_sectors = (size_mib * 1024 * 1024) / SECTOR_SIZE;

        // A previous failed run may have left the staging device
        // active. Tear it down so the fresh `thin::activate` below
        // doesn't trip over `EEXIST`. The matching staging thin id is
        // not persisted anywhere, so it leaks into pool metadata; that
        // is a bounded one-off cost and only `thin_dump` can find it.
        if let Ok(true) = dm::device_exists(&staging_dm) {
            let _ = thin::deactivate(&staging_dm);
        }

        // 1. Allocate a fresh staging thin and write the ext4 image.
        let staging_id = thin::allocate(&self.pool_name)?;
        let staging_dev =
            match thin::activate(&staging_dm, &self.pool_name, staging_id, size_sectors) {
                Ok(p) => p,
                Err(e) => {
                    let _ = thin::delete(&self.pool_name, staging_id);
                    return Err(e);
                }
            };

        // 2. dd the ext4 image onto the staging device.
        if let Err(e) = dd_image(image_path, &staging_dev) {
            let _ = thin::deactivate(&staging_dm);
            let _ = thin::delete(&self.pool_name, staging_id);
            return Err(e);
        }

        // 3. Snapshot the staging volume as the immutable base. Suspend
        //    the staging device first so the snapshot sees a coherent
        //    metadata commit; resume it on the way out either way.
        let base_id_result = thin::suspend(&staging_dm).and_then(|()| {
            let id = thin::allocate_snap(&self.pool_name, staging_id);
            let _ = thin::resume(&staging_dm);
            id
        });
        let base_id = match base_id_result {
            Ok(id) => id,
            Err(e) => {
                let _ = thin::deactivate(&staging_dm);
                let _ = thin::delete(&self.pool_name, staging_id);
                return Err(e);
            }
        };

        // 4. Drop the staging device + thin id; the base id retains all
        //    of its blocks.
        let _ = thin::deactivate(&staging_dm);
        let _ = thin::delete(&self.pool_name, staging_id);

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
        self.assert_pool_healthy()?;
        let base_id = Self::require_image_thin_id(image)?;

        let dm_name = thin::vm_dm_name(&self.vm_prefix, vm_name);
        // The VM's virtual size matches the image's size at clone time;
        // resize to a larger disk happens in a subsequent `resize` call.
        let size_sectors = (image.size_mib * 1024 * 1024) / SECTOR_SIZE;

        let vm_id = thin::allocate_snap(&self.pool_name, base_id)?;
        match thin::activate(&dm_name, &self.pool_name, vm_id, size_sectors) {
            Ok(disk_path) => Ok(VolumeHandle {
                disk_path,
                thin_id: Some(vm_id),
            }),
            Err(e) => {
                let _ = thin::delete(&self.pool_name, vm_id);
                Err(e)
            }
        }
    }

    fn resize(&self, vm: &VmMetadata, new_size: ByteSize) -> Result<()> {
        self.ensure_pool_active()?;
        self.assert_pool_healthy()?;
        let vm_id = Self::require_vm_thin_id(vm)?;
        let dm_name = thin::vm_dm_name(&self.vm_prefix, &vm.name);
        let new_sectors = new_size.bytes() / SECTOR_SIZE;

        // Activate (lazy) so we have a device to reload.
        let current_sectors = Self::vm_size_sectors(vm);
        let dev_path = self.ensure_thin_active(&dm_name, vm_id, current_sectors)?;

        thin::reload_size(&dm_name, &self.pool_name, vm_id, new_sectors)?;
        zvol::wait_for_device(&dev_path)?;
        e2fsck(&dev_path)?;
        resize2fs(&dev_path)?;
        Ok(())
    }

    fn destroy_vm_storage(&self, vm: &VmMetadata) -> Result<()> {
        // Best-effort: deactivate first, then free the thin id. Either
        // step may already be done by an earlier failure path.
        let _ = self.ensure_pool_active();
        let dm_name = thin::vm_dm_name(&self.vm_prefix, &vm.name);
        if let Ok(true) = dm::device_exists(&dm_name) {
            let _ = thin::deactivate(&dm_name);
        }
        if let Some(id) = vm.thin_id {
            let _ = thin::delete(&self.pool_name, id);
        }
        Ok(())
    }

    fn destroy_image_storage(&self, image: &ImageEntry, _force: bool) -> Result<()> {
        // dm-thin reference-counts blocks; deleting the base thin is
        // safe even when VMs still have clones — they keep their own
        // thin ids and stay readable. `force` doesn't change behavior.
        let _ = self.ensure_pool_active();
        let dm_name = thin::image_dm_name(&self.image_prefix, &image.local_name);
        if let Ok(true) = dm::device_exists(&dm_name) {
            let _ = thin::deactivate(&dm_name);
        }
        if let Some(id) = image.thin_id {
            let _ = thin::delete(&self.pool_name, id);
        }
        Ok(())
    }

    fn disk_device_path(&self, vm: &VmMetadata) -> Result<PathBuf> {
        // Ensure the pool table and the per-VM thin device are live in
        // the kernel. After a host reboot both are gone; without this,
        // `vm start` would hand Firecracker a stale `/dev/mapper/...`
        // path that resolves to ENOENT.
        self.ensure_pool_active()?;
        // Not covered by `ensure_pool_active`, which short-circuits
        // when the pool is already up. Handing a guest a device whose
        // every write returns EIO is worth one status call.
        if self.vdo.is_some() {
            vdo::assert_read_write(&self.vdo_name, &vdo::status(&self.vdo_name)?)?;
        }
        let thin_id = Self::require_vm_thin_id(vm)?;
        let dm_name = thin::vm_dm_name(&self.vm_prefix, &vm.name);
        let size_sectors = Self::vm_size_sectors(vm);
        self.ensure_thin_active(&dm_name, thin_id, size_sectors)
    }

    fn clone_vm_storage(&self, source: &VmMetadata, target_vm: &str) -> Result<VolumeHandle> {
        self.ensure_pool_active()?;
        self.assert_pool_healthy()?;
        let source_id = Self::require_vm_thin_id(source)?;
        let dm_name = thin::vm_dm_name(&self.vm_prefix, target_vm);
        let size_sectors = Self::vm_size_sectors(source);

        let fork_id = thin::allocate_snap(&self.pool_name, source_id)?;
        match thin::activate(&dm_name, &self.pool_name, fork_id, size_sectors) {
            Ok(disk_path) => Ok(VolumeHandle {
                disk_path,
                thin_id: Some(fork_id),
            }),
            Err(e) => {
                let _ = thin::delete(&self.pool_name, fork_id);
                Err(e)
            }
        }
    }

    /// Rename a VM's `/dev/mapper/<vm_prefix><name>` device.
    ///
    /// Uses `dmsetup rename` when the device is currently active;
    /// otherwise it's a no-op since the new dm name is constructed
    /// from the new VM name at lazy-activation time. The thin id —
    /// which is what fork snapshots/clones actually reference —
    /// stays the same.
    fn rename_vm_storage(&self, vm: &VmMetadata, new_name: &str) -> Result<VolumeHandle> {
        // Ensure the pool is up so `dmsetup` can see/rename the
        // device; otherwise rename would silently 'succeed' against
        // a missing device tree.
        self.ensure_pool_active()?;
        let old_dm = thin::vm_dm_name(&self.vm_prefix, &vm.name);
        let new_dm = thin::vm_dm_name(&self.vm_prefix, new_name);
        if dm::device_exists(&old_dm)? {
            thin::rename(&old_dm, &new_dm)?;
        }
        Ok(VolumeHandle {
            disk_path: thin::device_path(&new_dm),
            thin_id: vm.thin_id,
        })
    }

    /// Rename an image's `/dev/mapper/<image_prefix><name>` device.
    ///
    /// Image base devices are usually inactive (lazy activation), so
    /// this most often just produces the new path. When active, the
    /// rename is atomic via `dmsetup rename`. Clones of the image
    /// share blocks by thin id and are unaffected by the name change.
    fn rename_image_storage(
        &self,
        image: &ImageEntry,
        new_local_name: &str,
    ) -> Result<VolumeHandle> {
        self.ensure_pool_active()?;
        let old_dm = thin::image_dm_name(&self.image_prefix, &image.local_name);
        let new_dm = thin::image_dm_name(&self.image_prefix, new_local_name);
        if dm::device_exists(&old_dm)? {
            thin::rename(&old_dm, &new_dm)?;
        }
        Ok(VolumeHandle {
            disk_path: thin::device_path(&new_dm),
            thin_id: image.thin_id,
        })
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

    /// Pool figures come from the status line we already parse for
    /// health checks. Per-volume figures need a metadata snapshot, so
    /// the whole installation is measured under one reservation.
    ///
    /// Unlike the rest of the backend this does not activate the pool.
    /// Measuring is a query, and callers include `ember vm list`, which
    /// has no business loading a pool table, running `thin_check`, and
    /// attaching loop devices as a side effect of listing VMs.
    fn usage(&self, vms: &[VmMetadata], images: &[ImageEntry]) -> Result<StorageUsage> {
        if !dm::device_exists(&self.pool_name)? {
            return Err(Error::Pool(format!(
                "dm-thin pool '{}' is not active, so its usage cannot be measured. \
                 Any command that touches storage will activate it.",
                self.pool_name
            )));
        }
        let status = pool::status(&self.pool_name)?;
        let block_bytes = (self.block_size_sectors as u64) * SECTOR_SIZE;
        // A pool cannot be active without its data device, so a
        // configured VDO layer is necessarily up by now.
        let vdo_stats = match self.vdo {
            Some(_) => Some(vdo::stats(&self.vdo_name)?),
            None => None,
        };

        let by_id = {
            let metadata_loop = loop_device::find_for(&self.metadata_file())?.ok_or_else(|| {
                Error::Config(format!(
                    "metadata device {} is not attached to a loop device",
                    self.metadata_file().display()
                ))
            })?;
            let _snap = pool::MetadataSnap::reserve(&self.pool_name)?;
            tools::list_thins(&metadata_loop)?
        };

        Ok(StorageUsage {
            pool: pool_usage(&status, block_bytes, vdo_stats.as_ref()),
            vms: join_vms(vms, &by_id),
            images: join_images(images, &by_id),
        })
    }

    fn deinit(&self, purge: bool) -> Result<()> {
        // 1. Deactivate every thin volume that belongs to *this*
        //    installation so the pool can be removed cleanly. Other
        //    ember installs use distinct prefixes and stay untouched.
        for prefix in [&self.image_prefix, &self.vm_prefix] {
            for name in dm::list_with_prefix(prefix)? {
                let _ = thin::deactivate(&name);
            }
        }
        // 2. Drop the pool itself (if active).
        if dm::device_exists(&self.pool_name)? {
            dm::remove(&self.pool_name)?;
        }
        // 3. Drop the VDO layer, if this installation has one. Ordering
        //    matters: VDO holds the data loop device open until it goes
        //    away, so this has to happen before the detach below and
        //    after the pool that sits on top of it.
        if dm::device_exists(&self.vdo_name)? {
            vdo::remove(&self.vdo_name)?;
        }
        // 4. Detach the loop devices, if any.
        let metadata_path = self.metadata_file();
        let data_path = self.data_file();
        if let Some(loop_dev) = loop_device::find_for(&metadata_path)? {
            let _ = loop_device::detach(&loop_dev);
        }
        if let Some(loop_dev) = loop_device::find_for(&data_path)? {
            let _ = loop_device::detach(&loop_dev);
        }
        // 5. Optionally delete the backing files. A raw block device
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
        println!("dm-thin pool '{}' torn down.", self.pool_name);
        Ok(())
    }

    /// Grow the pool, and whatever sits under it.
    ///
    /// Three things have to stay consistent: the backing store, the VDO
    /// volume's two sizes when there is one, and the thin-pool table.
    /// They are grown bottom-up so no layer is told about space the
    /// layer beneath it does not have yet.
    fn grow(&self, request: GrowRequest) -> Result<()> {
        self.ensure_pool_active()?;

        let data_path = self.data_file();
        let metadata_loop = loop_device::find_for(&self.metadata_file())?.ok_or_else(|| {
            Error::Config(format!(
                "metadata device {} is not attached to a loop device",
                self.metadata_file().display()
            ))
        })?;
        let backing = if data_path.is_file() {
            loop_device::find_for(&data_path)?.ok_or_else(|| {
                Error::Config(format!(
                    "data device {} is not attached to a loop device",
                    data_path.display()
                ))
            })?
        } else {
            data_path.clone()
        };

        // Resolve and validate the whole request before touching
        // anything. A rejected grow that had already enlarged the
        // backing file would leave the disk footprint bigger with
        // nothing to show for it.
        let status = pool::status(&self.pool_name)?;
        let plan = plan_grow(
            &request,
            &GrowContext {
                vdo: self.vdo,
                backing_is_file: data_path.is_file(),
                backing_size: device_size_bytes(&backing)?,
                pool_capacity: status.total_data_blocks
                    * (self.block_size_sectors as u64)
                    * SECTOR_SIZE,
            },
        )?;
        if let (Some(old), Some(new)) = (self.vdo, plan.vdo) {
            vdo::check_growth(&self.vdo_params(old), &self.vdo_params(new))?;
        }

        if plan.resize_backing {
            create_sparse_file(&data_path, plan.physical_size)?;
            loop_device::refresh_size(&backing)?;
        }

        // With a compression layer, grow it before the pool: the
        // pool's data device *is* the VDO device, so it has to be the
        // larger one first.
        //
        // Without one, the capacity comes from the plan rather than
        // from the device. The two differ on a raw block device, which
        // is an upper bound the operator may deliberately not be using
        // all of, and reading it back would silently ignore `--size`.
        let (data_dev, data_sectors) = match plan.vdo {
            None => (backing, plan.physical_size / SECTOR_SIZE),
            Some(new) => {
                vdo::reload(&self.vdo_name, &backing, &self.vdo_params(new))?;
                // The kernel has now durably recorded the new sizes and
                // will demand them on every future startup, so the
                // config has to agree before anything else is allowed
                // to fail. Persisting after the pool reload instead
                // would leave a pool that cannot be activated again if
                // that reload were rejected.
                self.persist_vdo_config(new)?;
                println!(
                    "Grew VDO volume '{}' to {} physical, {} addressable.",
                    self.vdo_name,
                    format_bytes(new.physical_size),
                    format_bytes(new.logical_size),
                );
                // With a layer underneath, the pool's data device is
                // the VDO device and its size is the logical one.
                let dev = vdo::device_path(&self.vdo_name);
                let sectors = device_sectors(&dev)?;
                (dev, sectors)
            }
        };

        pool::reload(
            &self.pool_name,
            &metadata_loop,
            &data_dev,
            data_sectors,
            self.block_size_sectors,
            pool::DEFAULT_LOW_WATER_BLOCKS,
        )?;
        println!(
            "Grew dm-thin pool data capacity to {}.",
            format_bytes(data_sectors * SECTOR_SIZE)
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

/// Turn a thin-pool status line, and the VDO status underneath it when
/// there is one, into pool-level byte figures.
///
/// `dmsetup status` counts data in pool blocks and metadata in the
/// kernel's fixed 4 KiB metadata blocks, so the two need different
/// multipliers.
///
/// Whether the pool's own figures are physical depends on what is under
/// it. On bare storage they are. Over VDO they become the logical side,
/// and the physical truth has to come from VDO, which is the only layer
/// that knows how much disk the compressed blocks actually take.
fn pool_usage(
    status: &pool::PoolStatus,
    block_bytes: u64,
    vdo_stats: Option<&vdo::VdoStats>,
) -> PoolUsage {
    let thin_capacity = status.total_data_blocks * block_bytes;
    let thin_allocated = status.used_data_blocks * block_bytes;
    let metadata = Some(MetadataUsage {
        capacity: status.total_metadata_blocks * pool::METADATA_BLOCK_SIZE,
        used: status.used_metadata_blocks * pool::METADATA_BLOCK_SIZE,
    });
    match vdo_stats {
        // dm-thin never over-allocates against a volume's virtual size,
        // so nothing is reserved-but-empty the way a zvol is, and it
        // stores blocks verbatim, so there is no logical size to report
        // distinct from the allocated one. Reporting `None` rather than
        // a figure equal to `allocated` keeps the CLI from printing a
        // meaningless 1.00x ratio.
        None => PoolUsage {
            capacity: thin_capacity,
            allocated: thin_allocated,
            reserved: 0,
            logical: None,
            addressable: None,
            metadata,
        },
        // VDO's own metadata is charged to the pool but holds no data,
        // which is exactly what `reserved` is for. Folding it into
        // `allocated` alone would divide the compression ratio by a
        // constant several gigabytes wide and report roughly 1.00x on
        // any pool small enough to care.
        Some(v) => PoolUsage {
            capacity: v.total_bytes(),
            allocated: v.used_bytes(),
            reserved: v.overhead_bytes(),
            logical: Some(thin_allocated),
            addressable: Some(thin_capacity),
            metadata,
        },
    }
}

/// Project a `thin_ls` row onto a record's accounting.
///
/// Returns `None` for a record with no thin id, and for an id the pool
/// no longer knows about. Both are reported as absent rather than as an
/// empty volume, since zero bytes and "cannot say" are different
/// answers.
fn volume_usage(
    thin_id: Option<u64>,
    provisioned: u64,
    rows: &[tools::ThinRow],
) -> Option<VolumeUsage> {
    let thin_id = thin_id?;
    let row = rows.iter().find(|r| r.dev_id == thin_id)?;
    Some(VolumeUsage {
        provisioned,
        exclusive: row.exclusive_bytes,
        referenced: Some(row.mapped_bytes),
        logical: None,
    })
}

fn join_vms(vms: &[VmMetadata], rows: &[tools::ThinRow]) -> BTreeMap<String, VolumeUsage> {
    vms.iter()
        .filter_map(|vm| {
            let provisioned = DmThinStorage::vm_size_sectors(vm) * SECTOR_SIZE;
            Some((
                vm.name.clone(),
                volume_usage(vm.thin_id, provisioned, rows)?,
            ))
        })
        .collect()
}

fn join_images(images: &[ImageEntry], rows: &[tools::ThinRow]) -> BTreeMap<String, VolumeUsage> {
    images
        .iter()
        .filter_map(|img| {
            let provisioned = img.size_mib * 1024 * 1024;
            Some((
                img.local_name.clone(),
                volume_usage(img.thin_id, provisioned, rows)?,
            ))
        })
        .collect()
}

/// What a grow has to work with, separated from the request so the
/// decision logic can be tested without a kernel.
#[derive(Clone, Copy, Debug)]
struct GrowContext {
    /// The compression layer's currently recorded sizes, if any.
    vdo: Option<VdoConfig>,
    /// Whether the backing store is a sparse file ember can resize, as
    /// opposed to a block device somebody else owns.
    backing_is_file: bool,
    /// The backing store's actual size right now. An upper bound on a
    /// raw device, and the pool's own size on a file-backed one.
    backing_size: u64,
    /// The thin pool's current data capacity. Not the same as
    /// `backing_size` on a raw device the operator already grew
    /// externally, which is the case `grow` exists to pick up.
    pool_capacity: u64,
}

/// The resolved shape of a grow, before anything is touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GrowPlan {
    physical_size: u64,
    /// New compression-layer sizes, when the pool has a layer.
    vdo: Option<VdoConfig>,
    /// Whether the backing file needs enlarging first.
    resize_backing: bool,
}

/// Turn a grow request into a plan, rejecting everything that cannot
/// work before any of it is applied.
///
/// The baseline for "current size" is what the pool was told it has,
/// not what the device underneath happens to be. Those differ on a raw
/// device deliberately larger than the pool, and taking the device
/// would read a doubling as a shrink and let `--logical-size` swallow
/// the rest of the disk as a side effect.
fn plan_grow(request: &GrowRequest, ctx: &GrowContext) -> Result<GrowPlan> {
    if request.logical_size.is_some() && ctx.vdo.is_none() {
        return Err(Error::Config(
            "--logical-size applies only to a pool with a compression layer. \
             Without one the pool can hand out exactly the space it has, so use --size."
                .to_string(),
        ));
    }

    let old_physical = ctx.vdo.map_or(ctx.pool_capacity, |v| v.physical_size);
    let new_physical = match request.physical_size {
        Some(size) => vdo_aligned(size.bytes(), ctx.vdo.is_some()),
        // A raw device the operator already grew externally is the one
        // case where the new size can be discovered rather than stated.
        // Only when nothing else was asked for, though: `--logical-size`
        // alone must not swallow the rest of the device.
        None if !ctx.backing_is_file && request.logical_size.is_none() => {
            vdo_aligned(ctx.backing_size, ctx.vdo.is_some())
        }
        None => old_physical,
    };

    if new_physical < old_physical {
        return Err(Error::Config(format!(
            "the pool already has {} of physical space, and shrinking it would \
             destroy data. `ember storage grow` only grows.",
            format_bytes(old_physical),
        )));
    }
    if new_physical > ctx.backing_size && !ctx.backing_is_file {
        return Err(Error::Config(format!(
            "the backing block device is {}, so it cannot hold a {} pool, and ember \
             cannot resize a device it does not own. Grow it externally first \
             (lvextend, a cloud volume resize, and so on) and then re-run this command.",
            format_bytes(ctx.backing_size),
            format_bytes(new_physical),
        )));
    }

    let vdo = ctx.vdo.map(|old| VdoConfig {
        physical_size: new_physical,
        logical_size: match request.logical_size {
            Some(size) => vdo_aligned(size.bytes(), true),
            None => vdo::scale_logical(old.physical_size, old.logical_size, new_physical),
        },
        deduplication: old.deduplication,
    });

    if let (Some(new), Some(old)) = (vdo, ctx.vdo) {
        if new.logical_size < old.logical_size {
            return Err(Error::Config(format!(
                "the pool already hands out {}, and a compression layer cannot \
                 address less than it did. `ember storage grow` only grows.",
                format_bytes(old.logical_size),
            )));
        }
    }

    let grows_physical = new_physical > old_physical;
    let grows_logical = vdo
        .zip(ctx.vdo)
        .is_some_and(|(new, old)| new.logical_size > old.logical_size);
    if !grows_physical && !grows_logical {
        return Err(Error::Config(format!(
            "nothing to grow: the pool already has {} of physical space{}. Pass a \
             larger --size, or --logical-size on a pool with a compression layer.",
            format_bytes(old_physical),
            ctx.vdo
                .map(|v| format!(" and hands out {}", format_bytes(v.logical_size)))
                .unwrap_or_default(),
        )));
    }

    Ok(GrowPlan {
        physical_size: new_physical,
        vdo,
        // Against the backing store's real size, not the recorded
        // one. `create_sparse_file` truncates, and the two can drift
        // if a previous grow failed between the VDO reload and the
        // config write.
        resize_backing: ctx.backing_is_file && new_physical > ctx.backing_size,
    })
}

/// Round a size to a whole VDO block when a compression layer is
/// present, and leave it alone otherwise.
///
/// Only VDO cares: dm-thin rounds to its own block size internally, but
/// a VDO table built from a size that is not a whole 4 KiB block claims
/// a sector the formatted volume does not have, and the kernel answers
/// with a bare `EINVAL`.
fn vdo_aligned(bytes: u64, has_vdo: bool) -> u64 {
    if has_vdo {
        vdo::align_down(bytes)
    } else {
        bytes
    }
}

/// Physical size the dm-thin pool's data device will have at init.
///
/// Exposed for the CLI, which resolves the VDO sizes before calling
/// `init` so that the `InitConfig` it passes and the `GlobalConfig` it
/// persists cannot disagree about how big the pool is. Same resolution
/// `init` itself performs, so there is one answer rather than two.
pub fn pool_size_at_init(
    storage_path: &Path,
    state_dir: &Path,
    mode: DmThinMode,
    requested: Option<ByteSize>,
) -> Result<u64> {
    let (_, data_path) = resolve_init_paths(storage_path, state_dir, mode);
    resolve_pool_size(&data_path, mode, requested)
}

/// Physical size of the dm-thin pool's data device.
///
/// `--size` when the operator gave one, otherwise the raw device's own
/// size. A file-backed pool has no size to discover, so omitting
/// `--size` there is an error rather than a guess.
fn resolve_pool_size(
    data_path: &Path,
    mode: DmThinMode,
    requested: Option<ByteSize>,
) -> Result<u64> {
    match requested {
        Some(size) => Ok(size.bytes()),
        None => match mode {
            DmThinMode::RawDevice => device_size_bytes(data_path),
            DmThinMode::File => Err(Error::Config(
                "dm-thin --size is required when using a file-backed pool".to_string(),
            )),
        },
    }
}

/// VDO table parameters for a pool with the given block size.
///
/// `max_discard_blocks` is the pool block size in VDO's 4 KiB units, so
/// one pool-block discard reaches VDO as a single bio. The kernel's own
/// default of one block would split it into sixteen.
fn vdo_params(config: VdoConfig, block_size_sectors: u32) -> vdo::Params {
    vdo::Params {
        logical_size: config.logical_size,
        physical_size: config.physical_size,
        deduplication: config.deduplication,
        max_discard_blocks: (block_size_sectors as u64) * SECTOR_SIZE / vdo::BLOCK_SIZE,
    }
}

/// Format and activate the VDO layer, then assemble the thin pool on
/// top of whichever device ended up being the data side.
///
/// Split out of `init` so its failure path has exactly one place to
/// undo, rather than a cleanup block after every step.
fn assemble_stack(
    pool_name: &str,
    vdo_name: &str,
    vdo_config: Option<VdoConfig>,
    metadata_loop: &Path,
    data_path: &Path,
    block_size_sectors: u32,
) -> Result<()> {
    let backing = ensure_loop_or_block(data_path)?;
    let data_dev = match vdo_config {
        None => backing,
        Some(config) => {
            let index = vdo::IndexMemory::for_physical_size(config.physical_size);
            let params = vdo_params(config, block_size_sectors);
            let summary = vdo::format(&backing, config.logical_size, index)?;
            if !summary.is_empty() {
                println!("{summary}");
            }
            let path = vdo::activate(vdo_name, &backing, &params)?;
            zvol::wait_for_device(&path)?;
            println!(
                "VDO volume '{vdo_name}' active: {} physical, {} addressable, compression on, \
                 deduplication {}. Expect it to want around {} of RAM.",
                format_bytes(config.physical_size),
                format_bytes(config.logical_size),
                if config.deduplication { "on" } else { "off" },
                format_bytes(vdo::ram_estimate_bytes(&params, index)),
            );
            path
        }
    };
    let data_sectors = device_sectors(&data_dev)?;
    pool::create(
        pool_name,
        metadata_loop,
        &data_dev,
        data_sectors,
        block_size_sectors,
        pool::DEFAULT_LOW_WATER_BLOCKS,
    )
}

/// Undo a partially assembled stack, innermost first.
///
/// Every step is best-effort: this runs while an error is already on
/// its way out, and a cleanup failure that masked it would be worse
/// than the leak.
fn unwind_stack(vdo_name: &str, metadata_loop: &Path, data_path: &Path, created_data: bool) {
    if let Ok(true) = dm::device_exists(vdo_name) {
        let _ = vdo::remove(vdo_name);
    }
    if data_path.is_file() {
        if let Ok(Some(dev)) = loop_device::find_for(data_path) {
            let _ = loop_device::detach(&dev);
        }
        // Only a file this run created. Anything pre-existing is
        // somebody's data, and `vdoformat` refusing to touch it is the
        // behaviour we want.
        if created_data {
            let _ = fs::remove_file(data_path);
        }
    }
    let _ = loop_device::detach(metadata_loop);
}

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

    fn status(
        used_data: u64,
        total_data: u64,
        used_meta: u64,
        total_meta: u64,
    ) -> pool::PoolStatus {
        pool::PoolStatus {
            used_metadata_blocks: used_meta,
            total_metadata_blocks: total_meta,
            used_data_blocks: used_data,
            total_data_blocks: total_data,
            mode: pool::PoolMode::ReadWrite,
        }
    }

    /// Data blocks scale by the pool's block size, metadata blocks by
    /// the kernel's fixed 4 KiB. Mixing the two multipliers up would
    /// misreport metadata by a factor of 16 at the default block size.
    #[test]
    fn pool_usage_scales_data_and_metadata_separately() {
        let block_bytes = pool::DEFAULT_BLOCK_SIZE_SECTORS as u64 * SECTOR_SIZE;
        assert_eq!(block_bytes, 65536);

        let u = pool_usage(&status(100, 1000, 5, 2048), block_bytes, None);
        assert_eq!(u.allocated, 100 * 65536);
        assert_eq!(u.capacity, 1000 * 65536);
        assert_eq!(u.free(), 900 * 65536);
        assert_eq!(u.reserved, 0);
        assert_eq!(u.logical, None);
        assert_eq!(u.addressable, None);
        assert_eq!(u.compression_ratio(), None);

        let meta = u.metadata.expect("dm-thin has a metadata device");
        assert_eq!(meta.used, 5 * 4096);
        assert_eq!(meta.capacity, 2048 * 4096);
    }

    fn vdo_stats(data_blocks: u64, overhead_blocks: u64, physical_blocks: u64) -> vdo::VdoStats {
        vdo::VdoStats {
            data_blocks,
            overhead_blocks,
            physical_blocks,
        }
    }

    /// Over VDO the pool's own figures stop being physical. What the
    /// pool has handed out becomes the logical side, and the bytes
    /// actually on disk come from VDO, which is the only layer that
    /// knows how far the blocks compressed.
    #[test]
    fn pool_usage_over_vdo_reports_physical_from_below() {
        let block_bytes = pool::DEFAULT_BLOCK_SIZE_SECTORS as u64 * SECTOR_SIZE;
        // Pool: 400 MiB handed out of 1000 blocks. VDO: 200 MiB of data
        // plus 100 MiB of its own metadata, in a 2 GiB volume. So the
        // data compressed 2:1 and the metadata must not dilute that.
        let thin = status(6400, 16_000, 5, 2048);
        let v = vdo_stats(51_200, 25_600, 524_288);

        let u = pool_usage(&thin, block_bytes, Some(&v));
        assert_eq!(u.capacity, 2 * 1024 * 1024 * 1024);
        assert_eq!(u.allocated, 300 * 1024 * 1024);
        assert_eq!(u.reserved, 100 * 1024 * 1024);
        assert_eq!(u.occupied(), 200 * 1024 * 1024);
        assert_eq!(u.logical, Some(400 * 1024 * 1024));
        assert_eq!(u.addressable, Some(1000 * 1024 * 1024));
        assert_eq!(u.compression_ratio(), Some(2.0));
        // Free is real physical headroom, not the pool's idea of it.
        assert_eq!(u.free(), 2 * 1024 * 1024 * 1024 - 300 * 1024 * 1024);
        // Headroom at the addressable level is a different number.
        assert_eq!(u.addressable_free(), Some(600 * 1024 * 1024));
    }

    /// Regression, measured on a real 60 GiB pool: 6.66 GiB of logical
    /// data compressed to 2.64 GiB alongside 4.05 GiB of VDO metadata.
    /// Charging the metadata to `allocated` alone reported 1.00x and
    /// made the layer look useless.
    #[test]
    fn vdo_metadata_does_not_dilute_the_compression_ratio() {
        let block_bytes = pool::DEFAULT_BLOCK_SIZE_SECTORS as u64 * SECTOR_SIZE;
        let thin = status(109_150, 983_040, 1434, 8832);
        let v = vdo_stats(692_060, 1_061_683, 15_728_640);

        let u = pool_usage(&thin, block_bytes, Some(&v));
        let ratio = u.compression_ratio().expect("a ratio over real data");
        assert!(
            (2.4..2.7).contains(&ratio),
            "expected roughly 2.5x, got {ratio}"
        );
    }

    /// The metadata device never goes through VDO, so its figures are
    /// unaffected by what sits under the data device.
    #[test]
    fn pool_usage_metadata_is_unchanged_by_vdo() {
        let block_bytes = pool::DEFAULT_BLOCK_SIZE_SECTORS as u64 * SECTOR_SIZE;
        let thin = status(100, 1000, 5, 2048);
        let bare = pool_usage(&thin, block_bytes, None).metadata.unwrap();
        let over_vdo = pool_usage(&thin, block_bytes, Some(&vdo_stats(10, 5, 100)))
            .metadata
            .unwrap();
        assert_eq!(bare.used, over_vdo.used);
        assert_eq!(bare.capacity, over_vdo.capacity);
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    fn vdo_config(physical: u64, logical: u64) -> VdoConfig {
        VdoConfig {
            physical_size: physical,
            logical_size: logical,
            deduplication: false,
        }
    }

    fn file_ctx(vdo: Option<VdoConfig>, backing: u64) -> GrowContext {
        GrowContext {
            vdo,
            backing_is_file: true,
            backing_size: backing,
            pool_capacity: backing,
        }
    }

    fn raw_ctx(vdo: Option<VdoConfig>, device: u64, pool_capacity: u64) -> GrowContext {
        GrowContext {
            vdo,
            backing_is_file: false,
            backing_size: device,
            pool_capacity,
        }
    }

    fn request(physical: Option<u64>, logical: Option<u64>) -> GrowRequest {
        GrowRequest {
            physical_size: physical.map(ByteSize::from_bytes),
            logical_size: logical.map(ByteSize::from_bytes),
        }
    }

    #[test]
    fn plan_grow_without_vdo_just_resizes_the_backing_file() {
        let plan = plan_grow(&request(Some(64 * GIB), None), &file_ctx(None, 32 * GIB)).unwrap();
        assert_eq!(plan.physical_size, 64 * GIB);
        assert_eq!(plan.vdo, None);
        assert!(plan.resize_backing);
    }

    #[test]
    fn plan_grow_preserves_the_over_provision_ratio() {
        let ctx = file_ctx(Some(vdo_config(32 * GIB, 64 * GIB)), 32 * GIB);
        let plan = plan_grow(&request(Some(64 * GIB), None), &ctx).unwrap();
        let vdo = plan.vdo.unwrap();
        assert_eq!(vdo.physical_size, 64 * GIB);
        assert_eq!(vdo.logical_size, 128 * GIB);
    }

    /// A logical-only grow must not move physical space. The baseline
    /// is the recorded size, not the device's, or a raw device that is
    /// deliberately larger than the pool would be swallowed whole.
    #[test]
    fn plan_grow_logical_only_leaves_physical_alone() {
        let ctx = raw_ctx(Some(vdo_config(32 * GIB, 32 * GIB)), 500 * GIB, 32 * GIB);
        let plan = plan_grow(&request(None, Some(64 * GIB)), &ctx).unwrap();
        assert_eq!(plan.physical_size, 32 * GIB);
        assert_eq!(plan.vdo.unwrap().logical_size, 64 * GIB);
        assert!(!plan.resize_backing);
    }

    /// Same setup, and the reason it matters: with the device size as
    /// the baseline, doubling a 32 GiB pool on a 500 GiB device reads
    /// as a shrink and there is no `--size` that works.
    #[test]
    fn plan_grow_on_an_oversized_raw_device_is_not_a_shrink() {
        let ctx = raw_ctx(Some(vdo_config(32 * GIB, 32 * GIB)), 500 * GIB, 32 * GIB);
        let plan = plan_grow(&request(Some(64 * GIB), None), &ctx).unwrap();
        assert_eq!(plan.physical_size, 64 * GIB);
        assert!(!plan.resize_backing, "a raw device is not ours to resize");
    }

    /// Regression: `--size` has to reach the pool table. On a raw
    /// device nothing gets resized, so the only way the requested size
    /// takes effect is by being carried through the plan. Reading the
    /// device back instead silently hands out the whole disk.
    #[test]
    fn plan_grow_carries_the_requested_size_on_a_raw_device() {
        let ctx = raw_ctx(None, 500 * GIB, 100 * GIB);
        let plan = plan_grow(&request(Some(200 * GIB), None), &ctx).unwrap();
        assert_eq!(plan.physical_size, 200 * GIB);
        assert!(!plan.resize_backing);
        assert_ne!(
            plan.physical_size, ctx.backing_size,
            "the plan must not have fallen back to the device size"
        );
    }

    /// A compression layer cannot address less than it did, and the
    /// plan must not represent that state even briefly.
    #[test]
    fn plan_grow_refuses_a_logical_shrink() {
        let ctx = file_ctx(Some(vdo_config(32 * GIB, 64 * GIB)), 32 * GIB);
        let err = plan_grow(&request(Some(64 * GIB), Some(48 * GIB)), &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("only grows"), "{err}");
    }

    /// `create_sparse_file` truncates, so the decision to resize is
    /// made against the backing store's real size. The two drift if a
    /// grow failed between the VDO reload and the config write.
    #[test]
    fn plan_grow_never_shrinks_a_backing_file_that_ran_ahead() {
        // Config says 32 GiB, the file is already 64 GiB.
        let ctx = file_ctx(Some(vdo_config(32 * GIB, 32 * GIB)), 64 * GIB);
        let plan = plan_grow(&request(Some(40 * GIB), None), &ctx).unwrap();
        assert_eq!(plan.physical_size, 40 * GIB);
        assert!(
            !plan.resize_backing,
            "truncating to 40 GiB would shrink a 64 GiB file"
        );
    }

    /// A raw device the operator grew externally can be picked up with
    /// no `--size` at all, which is what the flag's help promises.
    #[test]
    fn plan_grow_picks_up_a_grown_raw_device() {
        let ctx = raw_ctx(None, 500 * GIB, 100 * GIB);
        let plan = plan_grow(&request(None, None), &ctx).unwrap();
        assert_eq!(plan.physical_size, 500 * GIB);
    }

    #[test]
    fn plan_grow_refuses_a_shrink() {
        let err = plan_grow(&request(Some(16 * GIB), None), &file_ctx(None, 32 * GIB))
            .unwrap_err()
            .to_string();
        assert!(err.contains("only grows"), "{err}");
    }

    #[test]
    fn plan_grow_refuses_to_outgrow_a_raw_device() {
        let ctx = raw_ctx(None, 32 * GIB, 32 * GIB);
        let err = plan_grow(&request(Some(64 * GIB), None), &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("resize a device it does not own"), "{err}");
    }

    #[test]
    fn plan_grow_refuses_logical_size_without_a_compression_layer() {
        let err = plan_grow(&request(None, Some(64 * GIB)), &file_ctx(None, 32 * GIB))
            .unwrap_err()
            .to_string();
        assert!(err.contains("compression layer"), "{err}");
    }

    /// A grow that changes nothing must say so rather than suspend the
    /// live stack and print two success lines.
    #[test]
    fn plan_grow_refuses_a_no_op() {
        for req in [
            request(None, None),
            request(Some(32 * GIB), None),
            request(Some(32 * GIB), Some(32 * GIB)),
        ] {
            let ctx = file_ctx(Some(vdo_config(32 * GIB, 32 * GIB)), 32 * GIB);
            let err = plan_grow(&req, &ctx).unwrap_err().to_string();
            assert!(err.contains("nothing to grow"), "{err}");
        }
    }

    /// Sizes that are not a whole VDO block are rounded rather than
    /// passed through to produce a table the kernel rejects.
    #[test]
    fn plan_grow_aligns_sizes_for_a_compressed_pool() {
        let ctx = file_ctx(Some(vdo_config(32 * GIB, 32 * GIB)), 32 * GIB);
        let odd = 64 * GIB + 1025;
        let plan = plan_grow(&request(Some(odd), None), &ctx).unwrap();
        assert_eq!(plan.physical_size % vdo::BLOCK_SIZE, 0);
        assert_eq!(plan.physical_size, 64 * GIB);
        // A pool with no layer has no such constraint.
        let plain = plan_grow(&request(Some(odd), None), &file_ctx(None, 32 * GIB)).unwrap();
        assert_eq!(plain.physical_size, odd);
    }

    /// One pool-block discard has to reach VDO as a single bio, or
    /// reclaim is split into a bio per 4 KiB.
    #[test]
    fn vdo_max_discard_matches_the_pool_block_size() {
        let config = VdoConfig {
            physical_size: 8 << 30,
            logical_size: 8 << 30,
            deduplication: false,
        };
        // Default 64 KiB pool block is 16 of VDO's 4 KiB blocks.
        assert_eq!(
            vdo_params(config, pool::DEFAULT_BLOCK_SIZE_SECTORS).max_discard_blocks,
            16
        );
        // A 1 MiB pool block is 256 of them.
        assert_eq!(vdo_params(config, 2048).max_discard_blocks, 256);
    }

    fn row(dev_id: u64, mapped: u64, exclusive: u64) -> tools::ThinRow {
        tools::ThinRow {
            dev_id,
            mapped_bytes: mapped,
            exclusive_bytes: exclusive,
        }
    }

    fn vm_record(name: &str, thin_id: Option<u64>, disk_size_gib: u32) -> VmMetadata {
        let mut m = VmMetadata::default_for_teardown();
        m.name = name.to_string();
        m.thin_id = thin_id;
        m.disk_size_gib = disk_size_gib;
        m
    }

    #[test]
    fn join_matches_records_to_rows_by_thin_id() {
        let rows = vec![row(42, 3000, 2000), row(7, 500, 500)];
        let vms = [vm_record("a", Some(42), 1)];

        let joined = join_vms(&vms, &rows);
        let a = joined.get("a").expect("matched by thin id");
        assert_eq!(a.exclusive, 2000);
        assert_eq!(a.referenced, Some(3000));
        assert_eq!(a.shared(), Some(1000));
        assert_eq!(a.provisioned, 1024 * 1024 * 1024);
    }

    /// A record the pool cannot account for is absent from the map, not
    /// present with zeroes. The CLI renders absent as `-`.
    #[test]
    fn join_omits_unaccountable_records() {
        let rows = vec![row(42, 3000, 2000)];
        let vms = [
            // Never got a thin id (ZFS record, or a half-created VM).
            vm_record("no-id", None, 1),
            // Has an id the pool no longer knows about.
            vm_record("stale", Some(999), 1),
        ];

        let joined = join_vms(&vms, &rows);
        assert!(joined.is_empty(), "{joined:?}");
    }

    /// Thin ids the pool holds but no record claims (leaked staging
    /// volumes, another install) contribute to the pool figure and must
    /// not invent rows.
    #[test]
    fn join_ignores_rows_no_record_claims() {
        let rows = vec![row(42, 3000, 2000), row(7, 500, 500)];
        assert_eq!(join_vms(&[vm_record("a", Some(42), 1)], &rows).len(), 1);
        assert!(join_images(&[], &rows).is_empty());
    }
}
