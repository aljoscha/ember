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
/// Backends fill in what they can measure. `exclusive` is the field
/// every backend can produce, and the one users act on: it is what
/// destroying the volume actually returns to the pool.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct VolumeUsage {
    /// Virtual size presented to the guest.
    pub provisioned: u64,
    /// Physical bytes only this volume references.
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
    pub fn ratio(&self) -> Option<f64> {
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
    /// Uncompressed size of `allocated`. `None` when the backend does
    /// not compress.
    pub logical: Option<u64>,
    /// Present only for backends that keep a separate metadata device.
    pub metadata: Option<MetadataUsage>,
}

impl PoolUsage {
    pub fn free(&self) -> u64 {
        self.capacity.saturating_sub(self.allocated)
    }

    /// Compression ratio over allocated space, when the backend
    /// compresses.
    pub fn ratio(&self) -> Option<f64> {
        ratio(self.logical, Some(self.allocated))
    }
}

/// Shared by [`VolumeUsage::ratio`] and [`PoolUsage::ratio`]. A zero
/// denominator yields `None` rather than an infinity that would render
/// as `inf` in the CLI.
fn ratio(logical: Option<u64>, physical: Option<u64>) -> Option<f64> {
    match (logical, physical) {
        (Some(logical), Some(physical)) if physical > 0 => Some(logical as f64 / physical as f64),
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
    /// dm-thin file-backed pools; ZFS/btrfs/APFS return an error since
    /// they manage capacity differently (or the user resizes individual
    /// VM disks via [`StorageBackend::resize`]).
    fn grow(&self, new_size: ByteSize) -> Result<()>;

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
