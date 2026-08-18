//! Backend trait definitions for VM, storage, and networking.
//!
//! Each trait is implemented by a platform-specific type:
//!   - **Linux**: Firecracker (KVM) + ZFS zvols + TAP/iptables
//!   - **macOS**: Apple Virtualization Framework (via `ember-vz`) + APFS clones + vmnet
//!
//! The active implementation is selected at compile time in the binary crate
//! and re-exported as type aliases (`Vm`, `Storage`, `Network`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::size::ByteSize;
use crate::config::{DmThinMode, GlobalConfig};
use crate::error::Result;
use crate::image::registry::ImageEntry;
use crate::state::vm::{NetworkInfo, VmMetadata};

// ---------------------------------------------------------------------------
// Common types returned by backend traits
// ---------------------------------------------------------------------------

/// Information returned when a VM is successfully started.
///
/// Encapsulates everything the CLI layer needs after a backend boots a VM:
/// the hypervisor process PID and the guest's network configuration.
pub struct StartedVm {
    /// PID of the hypervisor process (Firecracker on Linux, ember-vz on macOS).
    pub pid: u32,
    /// Network configuration for the running VM.
    pub network: NetworkInfo,
}

/// A storage volume returned by the [`StorageBackend`] when a fresh
/// volume is created (image base, VM clone, fork).
///
/// `disk_path` is what gets recorded on `VmMetadata::disk_path` /
/// `ImageEntry::disk_path` and passed to Firecracker as
/// `path_on_host`. `thin_id` is meaningful only for the dm-thin
/// backend; ZFS and macOS impls always return `None`.
pub struct VolumeHandle {
    pub disk_path: PathBuf,
    pub thin_id: Option<u64>,
}

impl VolumeHandle {
    /// Build a handle for backends that have no thin id concept.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self {
            disk_path: path.into(),
            thin_id: None,
        }
    }
}

/// Space accounting for a single volume, in bytes.
///
/// This is an occupancy model: it answers where space has gone, not
/// what a delete would give back. Those differ enough on ZFS to be
/// worth stating. A zvol is also charged for its refreservation and for
/// blocks held only by its snapshots, and neither appears in
/// `exclusive`. Backends whose forks are independent (dm-thin, APFS)
/// have no such gap.
///
/// Backends fill in what they can measure. `exclusive` is the only
/// field all of them produce.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct VolumeUsage {
    /// Virtual size presented to the guest.
    pub provisioned: u64,
    /// Physical bytes this volume holds that are not shared with an
    /// origin. Always within `referenced` when that is known.
    pub exclusive: u64,
    /// Physical bytes reachable from this volume, including blocks
    /// shared with an origin. `None` when the backend cannot tell
    /// shared and exclusive blocks apart.
    pub referenced: Option<u64>,
    /// Uncompressed size of `referenced`. `None` when the backend does
    /// not compress.
    pub logical: Option<u64>,
}

impl VolumeUsage {
    /// Bytes shared with an origin volume, when the backend can tell.
    pub fn shared(&self) -> Option<u64> {
        self.referenced.map(|r| r.saturating_sub(self.exclusive))
    }

    /// Compression ratio over the referenced blocks, when the backend
    /// compresses. `None` for an empty volume, where the ratio would be
    /// a division by zero rather than a meaningful 1.0.
    pub fn compression_ratio(&self) -> Option<f64> {
        ratio(self.logical, self.referenced)
    }
}

/// Usage of a backend's dedicated metadata device, in bytes.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct MetadataUsage {
    pub capacity: u64,
    pub used: u64,
}

/// Pool-wide capacity, in bytes.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct PoolUsage {
    pub capacity: u64,
    pub allocated: u64,
    /// Part of `allocated` that is reserved but holds no data, so it
    /// compresses to nothing and must be kept out of the ratio. Zero
    /// for backends without reservations.
    pub reserved: u64,
    /// Uncompressed size of the data within `allocated`. `None` when
    /// the backend does not compress.
    ///
    /// This doubles as the space handed out against
    /// [`addressable`](Self::addressable): the bytes callers have been
    /// given and the uncompressed bytes stored are the same number.
    pub logical: Option<u64>,
    /// Addressable space the pool exposes, when a compressing layer
    /// lets it exceed `capacity`. `None` when the pool can only hand
    /// out the physical space it actually has, which is every backend
    /// without such a layer.
    ///
    /// Deliberately not named after any one subsystem: it is a property
    /// of a pool that can promise more than it holds.
    pub addressable: Option<u64>,
    /// Present only for backends that keep a separate metadata device.
    pub metadata: Option<MetadataUsage>,
}

impl PoolUsage {
    pub fn free(&self) -> u64 {
        self.capacity.saturating_sub(self.allocated)
    }

    /// Bytes of `allocated` that actually hold data.
    pub fn occupied(&self) -> u64 {
        self.allocated.saturating_sub(self.reserved)
    }

    /// Compression ratio over occupied space, when the backend
    /// compresses.
    ///
    /// Measured against [`occupied`](Self::occupied) rather than
    /// `allocated`: empty reservation is charged to the pool but has no
    /// logical counterpart, so including it would understate the ratio.
    pub fn compression_ratio(&self) -> Option<f64> {
        ratio(self.logical, Some(self.occupied()))
    }

    /// Space still available to hand out, which is not the same as free
    /// physical space once a compressing layer is in play.
    ///
    /// `None` when the pool cannot over-promise, since `free` already
    /// answers the question there.
    pub fn addressable_free(&self) -> Option<u64> {
        let addressable = self.addressable?;
        Some(addressable.saturating_sub(self.logical.unwrap_or(0)))
    }
}

/// Shared by the two `compression_ratio` accessors.
///
/// Either side being zero yields `None`. A zero denominator would
/// render as `inf`, and a zero numerator is a pool that holds nothing
/// yet, where `0.00x` reads as catastrophic compression rather than as
/// the absence of data. A compressing layer's own metadata reserve
/// makes that state reachable on a pool that has only just been
/// created.
fn ratio(logical: Option<u64>, physical: Option<u64>) -> Option<f64> {
    match (logical, physical) {
        (Some(logical), Some(physical)) if logical > 0 && physical > 0 => {
            Some(logical as f64 / physical as f64)
        }
        _ => None,
    }
}

/// Space accounting for a whole installation, produced in one pass.
#[derive(Clone, Debug, Serialize)]
pub struct StorageUsage {
    pub pool: PoolUsage,
    /// Keyed by [`VmMetadata::name`]. A missing key means the backend
    /// could not account for that VM, which the CLI renders as `-`
    /// rather than as zero.
    pub vms: BTreeMap<String, VolumeUsage>,
    /// Keyed by [`ImageEntry::local_name`], same missing-key rule.
    pub images: BTreeMap<String, VolumeUsage>,
}

impl StorageUsage {
    /// Whether the per-volume figures were measured above a compressing
    /// layer the volumes cannot see into.
    ///
    /// True when the pool reports a logical size but no volume does,
    /// which is exactly the shape of a layer sitting below the pool:
    /// it has no idea which volume a physical block belongs to, so
    /// compression cannot be attributed per volume and every per-volume
    /// figure is pre-compression. A backend that compresses within each
    /// volume (ZFS) reports both and is not affected.
    pub fn volumes_above_compression(&self) -> bool {
        if self.pool.logical.is_none() {
            return false;
        }
        let mut volumes = self.vms.values().chain(self.images.values()).peekable();
        volumes.peek().is_some() && volumes.all(|v| v.logical.is_none())
    }
}

/// What `ember storage grow` was asked to change.
///
/// Two sizes, because a pool with a compressing layer beneath it has
/// two that can move independently: the real disk it sits on, and the
/// space it is willing to hand out. Backends without such a layer
/// accept only `physical_size`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrowRequest {
    /// New size for the pool's backing store. `None` leaves the disk
    /// footprint alone, which only makes sense together with
    /// `logical_size`.
    pub physical_size: Option<ByteSize>,
    /// New addressable size. `None` lets the backend derive one, which
    /// for a compressing pool means preserving the current ratio.
    pub logical_size: Option<ByteSize>,
}

/// Configuration for storage backend initialization during `ember init`.
///
/// Carries the subset of init arguments that the storage backend needs.
/// Platform-specific fields are ignored on backends that don't use them.
pub struct InitConfig {
    /// Selected storage backend. Drives the [`StorageBackend::init`]
    /// dispatch performed by `init_storage` in each platform crate.
    pub storage_backend: crate::config::StorageKind,
    /// Path to the state directory (e.g., `/var/lib/ember` or `~/Library/Application Support/ember`).
    pub state_dir: PathBuf,
    /// Per-installation namespace embedded in dm-thin pool / device
    /// names so `ember init` against a fresh state-dir doesn't trample
    /// another install's pool. Mirrors `GlobalConfig::instance_id`.
    pub instance_id: String,
    /// ZFS pool name. Used on Linux for `zfs create`; ignored on macOS.
    pub pool: String,
    /// Dataset name within the ZFS pool. Used on Linux; ignored on macOS.
    pub dataset: String,
    /// Block device for ZFS pool creation (e.g., `/dev/loop0`).
    /// Only used by the ZFS backend when creating a new pool.
    pub device: Option<String>,
    /// Backing path for non-ZFS backends.
    ///
    /// * btrfs: block device or sparse image file path.
    /// * dm-thin: directory for metadata.img/data.img, or a raw block device.
    pub storage_path: Option<PathBuf>,
    /// Size for the file-backed btrfs image (e.g., `"50G"`). When set, the
    /// btrfs backend treats `storage_path` as a sparse file to create.
    pub btrfs_size: Option<String>,
    /// Size of the dm-thin data device. Required for file-backed
    /// dm-thin pools, ignored for raw block devices.
    pub dm_thin_size: Option<ByteSize>,
    /// Override metadata device size for dm-thin. `None` lets the
    /// backend compute it via `thin_metadata_size`.
    pub dm_thin_metadata_size: Option<ByteSize>,
    /// dm-thin pool block size in 512-byte sectors. `None` uses the backend default.
    pub dm_thin_block_size: Option<u32>,
    /// dm-thin layout (file-backed vs raw-device). Resolved by the CLI
    /// from `storage_path` so the backend doesn't have to second-guess
    /// what the user supplied.
    pub dm_thin_mode: Option<DmThinMode>,
    /// dm-vdo compression layer to build beneath the dm-thin pool's
    /// data device. Sizes are already resolved, same as
    /// `dm_thin_block_size` and `dm_thin_mode`: the CLI works them out
    /// once so `InitConfig` and the persisted `GlobalConfig` cannot
    /// disagree.
    pub vdo: Option<crate::config::VdoConfig>,
}

// ---------------------------------------------------------------------------
// Backend traits
// ---------------------------------------------------------------------------

/// Hypervisor backend: manages VM processes.
///
/// - **Linux**: spawns and controls Firecracker via its API socket.
/// - **macOS**: spawns and signals the `ember-vz` Swift helper process.
///
/// All methods are associated functions (no `&self`). The correct implementation
/// is selected at compile time via `#[cfg(target_os)]` type aliases, so calls
/// look like `Vm::start(...)`.
pub trait VmBackend {
    /// Boot a VM. Returns the hypervisor PID and network info on success.
    ///
    /// On Linux: spawns Firecracker, configures it via API, sets up TAP + NAT.
    /// On macOS: spawns `ember-vz start`, waits for ready-fd, discovers guest IP.
    fn start(vm: &VmMetadata, config: &GlobalConfig) -> Result<StartedVm>;

    /// Graceful shutdown. Sends SIGTERM (or ACPI shutdown) and waits for exit.
    fn stop(vm: &VmMetadata) -> Result<()>;

    /// Forceful shutdown. Sends SIGKILL immediately.
    fn force_stop(vm: &VmMetadata) -> Result<()>;

    /// Pause the VM (freeze vCPUs).
    ///
    /// Linux: Firecracker Pause API. macOS: SIGUSR1 to ember-vz.
    fn pause(vm: &VmMetadata) -> Result<()>;

    /// Resume a paused VM.
    ///
    /// Linux: Firecracker Resume API. macOS: SIGUSR2 to ember-vz.
    fn resume(vm: &VmMetadata) -> Result<()>;

    /// Check whether a hypervisor process is still alive.
    ///
    /// Uses `kill(pid, 0)` — works the same on both platforms.
    fn is_running(pid: u32) -> bool;
}

/// Storage backend: manages disk images, clones, and forks.
///
/// - **Linux/ZFS**: ZFS zvols with `zfs clone`.
/// - **Linux/dm-thin**: device-mapper thin volumes with kernel `create_snap`.
/// - **macOS/APFS**: raw `.img` files with APFS CoW clones (`cp -c`).
///
/// Methods take `&VmMetadata` / `&ImageEntry` rather than bare names
/// for operations that need backend-specific state living on the
/// record (notably `thin_id` for dm-thin). Methods that *create* fresh
/// volumes return [`VolumeHandle`] so the caller can persist the new
/// `thin_id` (if any) on the matching record.
///
/// `init` is an associated function since it's called before the
/// backend is constructed.
pub trait StorageBackend {
    /// Initialize storage during `ember init`.
    fn init(config: &InitConfig) -> Result<()>
    where
        Self: Sized;

    /// Tear down the backend infrastructure created by [`init`].
    ///
    /// Inverse of `init`. The backend is responsible for unmounting,
    /// detaching, and (when `purge` is set) deleting backing files.
    /// Block devices supplied by the user are left intact in either
    /// case. The CLI removes `config.json` separately.
    fn deinit(&self, purge: bool) -> Result<()>;

    /// Grow the underlying pool capacity. Currently meaningful only for
    /// dm-thin pools; ZFS/btrfs/APFS return an error since they manage
    /// capacity differently (or the user resizes individual VM disks
    /// via [`StorageBackend::resize`]).
    ///
    /// A backend whose sizes are recorded in `GlobalConfig` is
    /// responsible for writing them back itself. It is the only layer
    /// that knows both the resolved values and the point in the
    /// sequence at which they become true, and getting that ordering
    /// wrong leaves a pool the next activation cannot open.
    fn grow(&self, request: GrowRequest) -> Result<()>;

    /// Create a base image volume from an ext4 image file.
    ///
    /// `name` is the image identifier (e.g., `library-alpine-latest`).
    /// `image_path` is the path to the ext4 image file to import.
    /// `size_mib` is the image size in MiB.
    ///
    /// Linux/ZFS: creates a zvol, writes the image via `dd`, creates `@base` snapshot.
    /// Linux/dm-thin: allocates a thin volume, writes the image, snaps it as the base id.
    /// macOS/APFS: copies the `.img` file into `images/data/`.
    fn create_image_volume(
        &self,
        name: &str,
        image_path: &Path,
        size_mib: u64,
    ) -> Result<VolumeHandle>;

    /// Clone a base image for a new VM.
    ///
    /// Linux/ZFS: `zfs clone <image>@base <pool>/.../vms/<vm_name>`.
    /// Linux/dm-thin: snapshot the image's base thin id into a fresh thin id.
    /// macOS/APFS: `cp -c <image>.img <vm>/rootfs.img`.
    fn clone_for_vm(&self, image: &ImageEntry, vm_name: &str) -> Result<VolumeHandle>;

    /// Resize a VM's disk to `new_size`. Caller is responsible for
    /// stopping the VM first.
    fn resize(&self, vm: &VmMetadata, new_size: ByteSize) -> Result<()>;

    /// Destroy all storage for a VM (disk image and any internal fork
    /// snapshots beneath it).
    fn destroy_vm_storage(&self, vm: &VmMetadata) -> Result<()>;

    /// Destroy storage for a base image.
    ///
    /// With `force: true`, also destroys any dependent storage (e.g.
    /// VM zvols cloned from this image) that couldn't be cleaned up at
    /// the application level — typically orphaned ZFS clones whose
    /// state files are already gone.
    fn destroy_image_storage(&self, image: &ImageEntry, force: bool) -> Result<()>;

    /// Mountable device path for a VM's root disk.
    ///
    /// Linux/ZFS: `/dev/zvol/pool/dataset/vms/vm_name`.
    /// Linux/dm-thin: `/dev/mapper/ember-<instance_id>-vm-<vm_name>`.
    /// macOS/APFS: `<state_dir>/vms/<vm_name>/rootfs.img`.
    ///
    /// Backends that lazily activate kernel state (notably dm-thin: pool
    /// table + per-VM thin device live only in kernel memory and are
    /// gone after a host reboot) must ensure the device is live before
    /// returning. Callers — `LinuxVm::start`, `vm create`, `vm fork` —
    /// rely on this so the path is immediately usable for `mount` /
    /// `open`.
    fn disk_device_path(&self, vm: &VmMetadata) -> Result<PathBuf>;

    /// Clone a VM's disk storage to create a new VM (used by `vm fork`).
    fn clone_vm_storage(&self, source: &VmMetadata, target_vm: &str) -> Result<VolumeHandle>;

    /// Rename a VM's disk storage from `vm.name` to `new_name`.
    ///
    /// Caller is responsible for ensuring the VM is stopped. Returns
    /// the new [`VolumeHandle`] with the updated `disk_path`; the
    /// `thin_id` (if any) is preserved. Any storage-level child
    /// references (e.g. fork snapshots / clones) keep working.
    fn rename_vm_storage(&self, vm: &VmMetadata, new_name: &str) -> Result<VolumeHandle>;

    /// Rename a base image's storage from `image.local_name` to
    /// `new_local_name`.
    ///
    /// Returns the new [`VolumeHandle`]; the `thin_id` (if any) is
    /// preserved. Dependent VM clones keep working.
    fn rename_image_storage(
        &self,
        image: &ImageEntry,
        new_local_name: &str,
    ) -> Result<VolumeHandle>;

    /// Clean up fork-related resources on the source VM.
    ///
    /// Used by ZFS to drop the per-fork snapshot it created on the
    /// source's dataset. No-op on backends where forks are independent
    /// (dm-thin, APFS).
    fn cleanup_fork(&self, parent: &VmMetadata, forked: &VmMetadata) -> Result<()>;

    /// VMs whose storage depends on `vm` and would break if `vm` were
    /// destroyed. Empty for backends whose forks are independent.
    fn storage_dependents(&self, vm: &VmMetadata) -> Result<Vec<String>>;

    /// Measure actual space usage across the installation.
    ///
    /// Takes the state records rather than discovering volumes itself,
    /// because the name-to-volume mapping lives in state and not in the
    /// backend. Returns the whole set in one value so that backends
    /// which have to walk pool-wide metadata do that walk once instead
    /// of once per volume.
    ///
    /// Volumes the backend cannot account for are left out of the maps
    /// instead of being reported as zero.
    fn usage(&self, vms: &[VmMetadata], images: &[ImageEntry]) -> Result<StorageUsage>;

    /// Mount a disk image and return the mount point path.
    ///
    /// Linux: mounts the zvol block device.
    /// macOS: not supported for ext4 — use [`inject_ssh_key`] instead.
    fn mount(&self, path: &Path) -> Result<PathBuf>;

    /// Unmount a previously mounted disk image.
    ///
    /// Linux: `umount`.
    /// macOS: not supported for ext4 — use [`inject_ssh_key`] instead.
    fn unmount(&self, mount_point: &Path) -> Result<()>;

    /// Inject an SSH public key into a VM's rootfs disk image.
    ///
    /// Detects whether the image has an ubuntu user and injects the key
    /// into the appropriate home directory. Returns the detected SSH user
    /// name (e.g., "root" or "ubuntu").
    ///
    /// Default implementation: mounts the image, injects the key via
    /// filesystem writes, then unmounts. macOS overrides this with
    /// `debugfs` since ext4 can't be mounted natively on macOS.
    fn inject_ssh_key(&self, image_path: &Path, pubkey_path: &Path) -> Result<String> {
        let mount_dir = self.mount(image_path)?;

        let inject_result = (|| -> Result<String> {
            let (user, home_relative) = crate::image::inject::detect_ssh_user(&mount_dir);
            crate::image::inject::inject_ssh_authorized_keys_for_home(
                &mount_dir,
                pubkey_path,
                home_relative,
            )?;
            Ok(user.to_string())
        })();

        let umount_result = self.unmount(&mount_dir);

        // Report inject error first, then unmount error.
        let user = inject_result?;
        umount_result?;

        Ok(user)
    }

    /// Inject the VM's hostname into `/etc/hosts` in the rootfs image.
    ///
    /// Adds the VM name to the loopback entries so that `sudo` and other
    /// tools can resolve the machine's own hostname without warnings.
    ///
    /// Default implementation: mounts the image, writes `/etc/hosts`,
    /// then unmounts. macOS overrides this with `debugfs`.
    fn inject_hostname(&self, image_path: &Path, hostname: &str) -> Result<()> {
        let mount_dir = self.mount(image_path)?;

        let inject_result = crate::image::inject::inject_hosts(&mount_dir, hostname);

        let umount_result = self.unmount(&mount_dir);

        inject_result?;
        umount_result?;

        Ok(())
    }
}

/// Network backend: manages VM networking.
///
/// - **Linux**: TAP devices + iptables NAT/masquerade + static IP allocation.
/// - **macOS**: vmnet shared mode (NAT + DHCP handled by the framework).
///
/// Methods use `&self` so the implementation can hold state (e.g., `StateStore`
/// for IP allocation tracking on Linux).
pub trait NetworkBackend {
    /// Set up networking for a VM. Returns the network configuration.
    ///
    /// Linux: allocates IP, creates TAP device, enables IP forwarding,
    /// adds iptables NAT rules.
    /// macOS: no-op (vmnet handles everything); returns vmnet gateway info.
    fn setup(&self, vm: &VmMetadata, config: &GlobalConfig) -> Result<NetworkInfo>;

    /// Tear down networking for a VM.
    ///
    /// Linux: removes the VM's iptables rules, deletes its TAP device,
    /// releases its IP.
    /// macOS: no-op (vmnet cleans up automatically).
    fn teardown(&self, vm: &VmMetadata, config: &GlobalConfig) -> Result<()>;

    /// Remove host-wide network state owned by this installation.
    ///
    /// Called from `ember deinit`, which refuses to run while any VM is
    /// registered, so no per-VM state is left to consider.
    ///
    /// Linux: removes the installation's firewall chains.
    ///
    /// Default: no-op, for backends that keep no host-wide state.
    fn deinit(&self, _config: &GlobalConfig) -> Result<()> {
        Ok(())
    }

    /// Discover the guest's IP address from its MAC address.
    ///
    /// Only meaningful on platforms where the guest IP is dynamically assigned
    /// (macOS vmnet DHCP). On Linux, IPs are statically allocated during
    /// [`setup`] and the caller never invokes this method.
    ///
    /// Default: returns an error indicating static allocation.
    fn discover_guest_ip(&self, _mac: &str) -> Result<String> {
        Err(crate::error::Error::Network(
            "guest IP discovery not supported — IPs are statically allocated".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Platform trait — covers everything not in Vm/Storage/Network backends
// ---------------------------------------------------------------------------

/// How to inject `/etc/resolv.conf` into a rootfs.
pub enum ResolvConfMode {
    /// Create a symlink to the given target (Linux: `/proc/net/pnp`).
    Symlink(&'static str),
    /// Write a static file with the given content (macOS: public DNS servers).
    StaticContent(&'static str),
}

/// Platform-specific tool configuration for OCI image pull/build.
pub struct ImageToolConfig {
    /// `tar` command name: `"tar"` on Linux, `"gtar"` on macOS.
    pub tar_command: &'static str,
    /// Whether `fakeroot` is needed (macOS non-root only).
    pub needs_fakeroot: bool,
    /// Override OS for skopeo multi-arch manifests. `Some("linux")` on macOS.
    pub override_os: Option<&'static str>,
    /// Generate a platform-appropriate install hint for a missing tool.
    pub install_hint: fn(&str) -> String,
}

/// Platform-level behaviors that don't belong in the VM/Storage/Network traits.
///
/// Covers lifecycle (root checks, reconciliation), display formatting,
/// image injection parameters, ext4 creation, and WAN detection.
/// Implemented by `LinuxPlatform` and `MacosPlatform` in the respective crates.
///
/// All methods are associated functions (no `&self`). The correct
/// implementation is selected at compile time via a type alias.
pub trait Platform {
    /// Whether this platform requires root for privileged operations.
    ///
    /// Linux: `true` (ZFS, TAP, iptables need root).
    /// macOS: `false` (vmnet, APFS clones run without root).
    ///
    /// The binary crate's `needs_root(command)` function decides *which*
    /// commands are privileged; this constant just says whether root
    /// matters at all on this platform.
    const REQUIRES_ROOT: bool;

    /// Run state reconciliation (clean up dead VMs, orphaned resources).
    fn reconcile(state_dir: &Path);

    /// Default state directory path.
    ///
    /// Linux: `/var/lib/ember`. macOS: `~/Library/Application Support/ember`.
    fn default_state_dir() -> PathBuf;

    /// Default IP subnet handed to `GlobalConfig.ip_subnet` at
    /// `ember init` when the user doesn't pass `--ip-subnet`.
    ///
    /// Linux carves a `/16` slot inside `10.0.0.0/8` and uses /30
    /// blocks per VM (host has full control of routing), scaling to
    /// ~16k VMs per install. macOS sub-allocates a `/27` inside
    /// vmnet's host-wide `192.168.64.0/24` and uses single-IP
    /// allocation (vmnet's shared L2 bridge means /30 P2P links are
    /// pointless), giving ~30 VMs per install. A `/8` collision
    /// between two installs is unlikely (1/8 per pair) and
    /// resolvable via the `--ip-subnet` override.
    fn default_ip_subnet(instance_id: &str) -> String;

    /// Console device name for inittab injection.
    ///
    /// Linux/Firecracker: `"ttyS0"`. macOS/AVF: `"hvc0"`.
    fn console_device() -> &'static str;

    /// How to configure `/etc/resolv.conf` in injected images.
    fn resolv_conf_mode() -> ResolvConfMode;

    /// Platform-specific tool configuration for OCI image pull/build.
    fn image_tool_config() -> ImageToolConfig;

    /// Platform-specific hint shown when ember is not initialized.
    fn init_hint() -> &'static str;

    /// Extra fields to display in `vm inspect` table output.
    fn inspect_vm_extra(metadata: &VmMetadata) -> Vec<(&'static str, String)>;

    /// Extra fields to display in `image inspect` table output.
    fn inspect_image_extra(entry: &ImageEntry) -> Vec<(&'static str, String)>;

    /// Extra fields to display in `ember info` output.
    fn info_extra(config: &GlobalConfig) -> Vec<(&'static str, String)>;

    /// Pre-pause/resume validation.
    ///
    /// Linux: checks Firecracker API socket exists. macOS: no-op.
    fn pre_pause_check(metadata: &VmMetadata) -> anyhow::Result<()>;

    /// Post-delete cleanup hook.
    ///
    /// Linux: `udevadm settle`. macOS: no-op.
    fn post_delete_cleanup();

    /// Detect the WAN interface, or use a user-provided override.
    ///
    /// Returns `(resolved_iface, messages_to_print)`.
    fn detect_wan_iface(user_provided: Option<&str>) -> (Option<String>, Vec<String>);

    /// Create an ext4 filesystem image from a rootfs directory.
    fn create_ext4_image(rootfs_dir: &Path, image_path: &Path, size_mib: u64) -> Result<()>;

    /// Estimate the ext4 image size needed to hold a rootfs directory.
    fn estimate_ext4_size_mib(rootfs_dir: &Path) -> Result<u64>;

    /// Total host RAM in MiB.
    ///
    /// Used by `ember vm start` admission control. Linux reads
    /// `/proc/meminfo`; macOS shells out to `sysctl hw.memsize`.
    /// Returns an error if the OS-specific source can't be read or parsed;
    /// callers are expected to soft-fail rather than block on this.
    fn host_ram_mib() -> anyhow::Result<u32>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(exclusive: u64, referenced: Option<u64>, logical: Option<u64>) -> VolumeUsage {
        VolumeUsage {
            provisioned: 1024,
            exclusive,
            referenced,
            logical,
        }
    }

    #[test]
    fn shared_is_the_gap_between_referenced_and_exclusive() {
        assert_eq!(volume(80, Some(100), None).shared(), Some(20));
        assert_eq!(volume(100, Some(100), None).shared(), Some(0));
    }

    /// Backends that cannot separate shared from exclusive report
    /// nothing rather than claiming zero sharing.
    #[test]
    fn shared_is_unknown_without_referenced() {
        assert_eq!(volume(80, None, None).shared(), None);
    }

    /// `exclusive` is contractually within `referenced`, but the
    /// saturating subtraction keeps a backend bug from producing a
    /// wrapped, astronomically large shared figure.
    #[test]
    fn shared_saturates_instead_of_wrapping() {
        assert_eq!(volume(120, Some(100), None).shared(), Some(0));
    }

    #[test]
    fn compression_ratio_divides_logical_by_referenced() {
        let r = volume(80, Some(100), Some(200)).compression_ratio();
        assert_eq!(r, Some(2.0));
    }

    /// An untouched volume would divide by zero. `None` renders as `-`
    /// where an infinity would render as `inf`.
    #[test]
    fn compression_ratio_guards_empty_volume() {
        assert_eq!(volume(0, Some(0), Some(0)).compression_ratio(), None);
        assert_eq!(volume(0, None, Some(200)).compression_ratio(), None);
        assert_eq!(volume(0, Some(100), None).compression_ratio(), None);
    }

    fn usage(pool: PoolUsage, volumes: &[VolumeUsage]) -> StorageUsage {
        StorageUsage {
            pool,
            vms: volumes
                .iter()
                .enumerate()
                .map(|(i, v)| (format!("vm{i}"), *v))
                .collect(),
            images: BTreeMap::new(),
        }
    }

    /// A layer below the pool cannot say which volume a block belongs
    /// to, so the pool reports a logical size and no volume does. That
    /// exact shape is what the CLI footnote keys off.
    #[test]
    fn volumes_above_compression_detects_a_layer_below_the_pool() {
        let compressed_pool = pool(400, 0, Some(800));
        let opaque = volume(1000, Some(400), None);
        assert!(usage(compressed_pool, &[opaque]).volumes_above_compression());
    }

    /// A backend that compresses within each volume reports both, and
    /// its per-volume figures need no disclaimer.
    #[test]
    fn volumes_above_compression_is_false_when_volumes_report_their_own() {
        let compressed_pool = pool(400, 0, Some(800));
        let transparent = volume(1000, Some(400), Some(800));
        assert!(!usage(compressed_pool, &[transparent]).volumes_above_compression());
        // Mixed is not the shape either: one volume knowing is enough.
        let mixed = [
            volume(1000, Some(400), Some(800)),
            volume(1000, Some(1), None),
        ];
        assert!(!usage(compressed_pool, &mixed).volumes_above_compression());
    }

    /// A pool that does not compress at all has nothing to disclaim,
    /// whatever its volumes report.
    #[test]
    fn volumes_above_compression_is_false_without_a_compressing_pool() {
        let plain = pool(400, 0, None);
        assert!(!usage(plain, &[volume(1000, Some(400), None)]).volumes_above_compression());
    }

    /// An install with no VMs and no images has no per-volume figures
    /// to qualify, so there is nothing to say.
    #[test]
    fn volumes_above_compression_is_false_with_no_volumes() {
        assert!(!usage(pool(400, 0, Some(800)), &[]).volumes_above_compression());
    }

    #[test]
    fn addressable_free_is_absent_unless_the_pool_over_promises() {
        assert_eq!(pool(400, 0, None).addressable_free(), None);
    }

    /// Headroom at the addressable level is measured against what has
    /// been handed out, which is the logical figure, not the physical
    /// bytes those blocks compressed down to.
    #[test]
    fn addressable_free_counts_what_is_still_unhanded_out() {
        let mut p = pool(400, 0, Some(800));
        p.addressable = Some(2000);
        assert_eq!(p.addressable_free(), Some(1200));
        assert_eq!(p.free(), 600);

        // Fully handed out, and not wrapping past it.
        p.logical = Some(2500);
        assert_eq!(p.addressable_free(), Some(0));
    }

    /// A pool that holds nothing has no ratio. Reporting `0.00x` reads
    /// as catastrophic compression, and a compressing layer's own
    /// metadata reserve makes that state reachable the moment a pool is
    /// created.
    #[test]
    fn an_empty_compressing_pool_reports_no_ratio() {
        let mut p = pool(3_500, 0, Some(0));
        p.addressable = Some(8_000);
        assert_eq!(p.compression_ratio(), None);
        assert_eq!(volume(1000, Some(400), Some(0)).compression_ratio(), None);
    }

    fn pool(allocated: u64, reserved: u64, logical: Option<u64>) -> PoolUsage {
        PoolUsage {
            capacity: 1000,
            allocated,
            reserved,
            logical,
            addressable: None,
            metadata: None,
        }
    }

    #[test]
    fn free_is_capacity_minus_allocated() {
        assert_eq!(pool(400, 0, None).free(), 600);
        // A pool reporting more allocated than capacity must not wrap.
        assert_eq!(pool(1200, 0, None).free(), 0);
    }

    /// Empty reservation is charged to the pool but has no logical
    /// counterpart, so leaving it in the denominator understates
    /// compression.
    #[test]
    fn pool_ratio_excludes_reservation() {
        let p = pool(300, 100, Some(400));
        assert_eq!(p.occupied(), 200);
        assert_eq!(p.compression_ratio(), Some(2.0));
    }

    #[test]
    fn pool_ratio_guards_fully_reserved_pool() {
        assert_eq!(pool(100, 100, Some(0)).compression_ratio(), None);
        assert_eq!(pool(0, 0, Some(0)).compression_ratio(), None);
        assert_eq!(pool(100, 0, None).compression_ratio(), None);
    }
}
