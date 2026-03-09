//! Platform-specific backend traits for VM, storage, and networking.
//!
//! Each trait is implemented by a platform-specific type:
//!   - **Linux**: Firecracker (KVM) + ZFS zvols + TAP/iptables
//!   - **macOS**: Apple Virtualization Framework (via `ember-vz`) + APFS clones + vmnet
//!
//! The active implementation is selected at compile time via `#[cfg(target_os)]`
//! and re-exported as type aliases (`Vm`, `Storage`, `Network`).

use std::path::{Path, PathBuf};

use crate::cli::init::GlobalConfig;
use crate::config::size::ByteSize;
use crate::error::Result;
use crate::state::vm::{NetworkInfo, VmMetadata};

// Platform-specific implementations.
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

// Type aliases for the active platform backend.
// Selected at compile time based on target OS.
#[cfg(target_os = "linux")]
pub type Vm = linux::LinuxVm;
#[cfg(target_os = "linux")]
pub type Storage = linux::LinuxStorage;
#[cfg(target_os = "linux")]
pub type Network = linux::LinuxNetwork;

#[cfg(target_os = "macos")]
pub type Vm = macos::MacosVm;
#[cfg(target_os = "macos")]
pub type Storage = macos::MacosStorage;
#[cfg(target_os = "macos")]
pub type Network = macos::MacosNetwork;

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

    /// Clone a VM snapshot to create a new VM's disk (used by `vm fork`).
    ///
    /// Creates a snapshot of the source VM (if `snap_name` doesn't exist yet),
    /// then clones that snapshot into a new VM.
    ///
    /// Returns `(disk_path, fork_snapshot_identifier)` where the identifier
    /// is stored in metadata for cleanup when the forked VM is deleted.
    ///
    /// Linux: `zfs clone pool/.../vms/source@snap pool/.../vms/target`.
    /// macOS: `cp -c vms/source/rootfs.img vms/target/rootfs.img`.
    fn clone_from_snapshot(
        &self,
        source_vm: &str,
        snap_name: &str,
        target_vm: &str,
    ) -> Result<(PathBuf, String)>;

    /// Clean up the fork origin snapshot created by [`clone_from_snapshot`].
    ///
    /// Called when deleting a forked VM to remove the snapshot that was
    /// created on the source VM during forking.
    fn destroy_fork_origin(&self, fork_origin: &str) -> Result<()>;

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
    /// Linux: the IP is statically assigned, so this is a no-op/lookup.
    /// macOS: parses `/var/db/dhcpd_leases` for the vmnet DHCP lease,
    /// with ARP-based fallback.
    fn discover_guest_ip(&self, mac: &str) -> Result<String>;
}
