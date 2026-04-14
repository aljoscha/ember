//! Backend trait definitions for VM, storage, and networking.
//!
//! Each trait is implemented by a platform-specific type:
//!   - **Linux**: Firecracker (KVM) + ZFS zvols + TAP/iptables
//!   - **macOS**: Apple Virtualization Framework (via `ember-vz`) + APFS clones + vmnet
//!
//! The active implementation is selected at compile time in the binary crate
//! and re-exported as type aliases (`Vm`, `Storage`, `Network`).

use std::path::{Path, PathBuf};

use crate::config::size::ByteSize;
use crate::config::GlobalConfig;
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

/// Platform-agnostic snapshot information.
///
/// On Linux this is backed by ZFS snapshots (`zfs list -t snapshot`).
/// On macOS this is backed by APFS clone files in the VM's `snapshots/` directory.
pub struct SnapshotInfo {
    /// Snapshot name (e.g., "snap1"). Does not include dataset path or directory prefix.
    pub name: String,
    /// Creation timestamp (Unix epoch seconds).
    pub created_at: u64,
    /// Size in bytes.
    ///
    /// - Linux/ZFS: `referenced` property (bytes the snapshot points to).
    /// - macOS/APFS: logical file size via `stat`.
    pub size: u64,
}

/// Configuration for storage backend initialization during `ember init`.
///
/// Carries the subset of init arguments that the storage backend needs.
/// Platform-specific fields (like ZFS pool/dataset) are ignored on platforms
/// that don't use them.
pub struct InitConfig {
    /// Path to the state directory (e.g., `/var/lib/ember` or `~/Library/Application Support/ember`).
    pub state_dir: PathBuf,
    /// ZFS pool name. Used on Linux for `zfs create`; ignored on macOS.
    pub pool: String,
    /// Dataset name within the ZFS pool. Used on Linux; ignored on macOS.
    pub dataset: String,
    /// Block device for ZFS pool creation (e.g., `/dev/loop0`).
    /// Only used on Linux when creating a new pool.
    pub device: Option<String>,
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

/// Storage backend: manages disk images, clones, and snapshots.
///
/// - **Linux**: ZFS zvols with snapshots and `zfs clone`.
/// - **macOS**: raw `.img` files with APFS CoW clones (`cp -c`).
///
/// Methods use `&self` so the implementation can hold platform-specific config
/// (e.g., ZFS pool/dataset paths on Linux, state directory on macOS).
/// `init` is an associated function since it's called before the backend is constructed.
pub trait StorageBackend {
    /// Initialize storage during `ember init`.
    ///
    /// Linux: creates ZFS pool (if needed) and datasets.
    /// macOS: validates the state directory is on an APFS volume.
    fn init(config: &InitConfig) -> Result<()>
    where
        Self: Sized;

    /// Create a base image volume from an ext4 image file.
    ///
    /// `name` is the image identifier (e.g., `library-alpine-latest`).
    /// `image_path` is the path to the ext4 image file to import.
    /// `size_mib` is the image size in MiB (used for zvol creation on Linux).
    ///
    /// Returns the zvol path (Linux) or .img file path (macOS).
    ///
    /// Linux: creates a zvol, writes the image via `dd`, creates `@base` snapshot.
    /// macOS: copies the `.img` file into `images/data/`.
    fn create_image_volume(&self, name: &str, image_path: &Path, size_mib: u64) -> Result<PathBuf>;

    /// Clone a base image for a new VM. Returns the zvol path (Linux) or
    /// .img file path (macOS).
    ///
    /// Linux: `zfs clone pool/.../images/name@base pool/.../vms/vm_name`.
    /// macOS: `cp -c images/data/name.img vms/vm_name/rootfs.img`.
    fn clone_for_vm(&self, image_name: &str, vm_name: &str) -> Result<PathBuf>;

    /// Create a named snapshot of a VM's current disk state.
    ///
    /// Linux: `zfs snapshot pool/.../vms/vm_name@snap_name`.
    /// macOS: `cp -c vms/vm_name/rootfs.img vms/vm_name/snapshots/snap_name.img`.
    fn snapshot(&self, vm_name: &str, snap_name: &str) -> Result<()>;

    /// Restore a VM's disk to a previously created snapshot.
    ///
    /// Linux: `zfs rollback pool/.../vms/vm_name@snap_name`.
    /// macOS: `cp -c vms/vm_name/snapshots/snap_name.img vms/vm_name/rootfs.img`.
    fn restore_snapshot(&self, vm_name: &str, snap_name: &str) -> Result<()>;

    /// Delete a snapshot.
    ///
    /// Linux: `zfs destroy pool/.../vms/vm_name@snap_name`.
    /// macOS: `rm vms/vm_name/snapshots/snap_name.img`.
    fn delete_snapshot(&self, vm_name: &str, snap_name: &str) -> Result<()>;

    /// List all snapshots for a VM.
    fn list_snapshots(&self, vm_name: &str) -> Result<Vec<SnapshotInfo>>;

    /// Resize a VM's disk to `new_size`.
    ///
    /// Linux: `zfs set volsize=... + resize2fs`.
    /// macOS: `truncate -s ... + resize2fs`.
    fn resize(&self, vm_name: &str, new_size: ByteSize) -> Result<()>;

    /// Destroy all storage for a VM (disk image, snapshots).
    ///
    /// Linux: `zfs destroy -r pool/.../vms/vm_name`.
    /// macOS: `rm -rf vms/vm_name/` (disk files only; state is separate).
    fn destroy_vm_storage(&self, vm_name: &str) -> Result<()>;

    /// Destroy storage for a base image.
    ///
    /// Linux: `zfs destroy pool/.../images/name` (and its @base snapshot).
    /// macOS: `rm images/data/name.img`.
    fn destroy_image_storage(&self, name: &str) -> Result<()>;

    /// Get the mountable device path for a VM's root disk.
    ///
    /// Linux: `/dev/zvol/pool/dataset/vms/vm_name` (block device for the zvol).
    /// macOS: `state_dir/vms/vm_name/rootfs.img` (raw disk image file).
    fn disk_device_path(&self, vm_name: &str) -> PathBuf;

    /// Clone a VM's disk storage to create a new VM (used by `vm fork`).
    ///
    /// Returns the disk path for the new VM.
    ///
    /// On Linux, this creates a ZFS snapshot on the source VM and clones it.
    /// The snapshot naming convention is internal to the backend.
    /// On macOS, this does a direct `cp -c` (APFS CoW clone) — no intermediate
    /// snapshot, no dependency between source and target.
    fn clone_vm_storage(&self, source_vm: &str, target_vm: &str) -> Result<PathBuf>;

    /// Clean up fork-related resources on the source VM.
    ///
    /// Called when deleting a forked VM to remove any backend-specific
    /// resources (e.g., ZFS snapshot on the source VM). The backend
    /// reconstructs the resource name from the parent/forked VM names.
    ///
    /// No-op on backends where forks are independent (e.g., macOS/APFS).
    fn cleanup_fork(&self, parent_vm: &str, forked_vm: &str) -> Result<()>;

    /// Check if deleting this VM would break other VMs' storage.
    ///
    /// Returns the names of VMs whose storage depends on this VM
    /// (e.g., ZFS clones that reference snapshots on this VM's dataset).
    /// An empty vec means the VM can be safely deleted.
    ///
    /// On Linux/ZFS, fork snapshots create a real dependency chain.
    /// On macOS/APFS, forks are independent — always returns empty.
    fn storage_dependents(&self, vm_name: &str) -> Result<Vec<String>>;

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
    /// Linux: removes iptables rules, deletes TAP device, releases IP.
    /// macOS: no-op (vmnet cleans up automatically).
    fn teardown(&self, vm: &VmMetadata) -> Result<()>;

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
}
