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
// Uncomment as each platform module is created:
//
// #[cfg(target_os = "linux")]
// pub mod linux;
// #[cfg(target_os = "linux")]
// pub use linux::{LinuxVm as Vm, LinuxStorage as Storage, LinuxNetwork as Network};
//
// #[cfg(target_os = "macos")]
// pub mod macos;
// #[cfg(target_os = "macos")]
// pub use macos::{MacosVm as Vm, MacosStorage as Storage, MacosNetwork as Network};

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
/// All methods are associated functions selected at compile time.
pub trait StorageBackend {
    /// Initialize storage during `ember init`.
    ///
    /// Linux: creates ZFS datasets (`pool/dataset/images`, `pool/dataset/vms`).
    /// macOS: validates the state directory is on an APFS volume.
    fn init(config: &InitConfig) -> Result<()>;

    /// Create a base image volume from an unpacked rootfs.
    ///
    /// `name` is the image identifier (e.g., `library-alpine-latest`).
    /// `image_path` is the path to the ext4 image file to import.
    ///
    /// Returns the path to the created volume/image.
    ///
    /// Linux: creates a zvol, writes the image via `dd`, creates `@base` snapshot.
    /// macOS: copies the `.img` file into `images/data/`.
    fn create_image_volume(name: &str, image_path: &Path) -> Result<PathBuf>;

    /// Clone a base image for a new VM. Returns the path to the VM's disk.
    ///
    /// Linux: `zfs clone pool/.../images/name@base pool/.../vms/vm_name`.
    /// macOS: `cp -c images/data/name.img vms/vm_name/rootfs.img`.
    fn clone_for_vm(image_name: &str, vm_name: &str) -> Result<PathBuf>;

    /// Create a named snapshot of a VM's current disk state.
    ///
    /// Linux: `zfs snapshot pool/.../vms/vm_name@snap_name`.
    /// macOS: `cp -c vms/vm_name/rootfs.img vms/vm_name/snapshots/snap_name.img`.
    fn snapshot(vm_name: &str, snap_name: &str) -> Result<()>;

    /// Restore a VM's disk to a previously created snapshot.
    ///
    /// Linux: `zfs rollback pool/.../vms/vm_name@snap_name`.
    /// macOS: `cp -c vms/vm_name/snapshots/snap_name.img vms/vm_name/rootfs.img`.
    fn restore_snapshot(vm_name: &str, snap_name: &str) -> Result<()>;

    /// Delete a snapshot.
    ///
    /// Linux: `zfs destroy pool/.../vms/vm_name@snap_name`.
    /// macOS: `rm vms/vm_name/snapshots/snap_name.img`.
    fn delete_snapshot(vm_name: &str, snap_name: &str) -> Result<()>;

    /// List all snapshots for a VM.
    fn list_snapshots(vm_name: &str) -> Result<Vec<SnapshotInfo>>;

    /// Resize a VM's disk to `new_size`.
    ///
    /// Linux: `zfs set volsize=... + resize2fs`.
    /// macOS: `truncate -s ... + resize2fs`.
    fn resize(vm_name: &str, new_size: ByteSize) -> Result<()>;

    /// Destroy all storage for a VM (disk image, snapshots).
    ///
    /// Linux: `zfs destroy -r pool/.../vms/vm_name`.
    /// macOS: `rm -rf vms/vm_name/` (disk files only; state is separate).
    fn destroy_vm_storage(vm_name: &str) -> Result<()>;

    /// Destroy storage for a base image.
    ///
    /// Linux: `zfs destroy pool/.../images/name` (and its @base snapshot).
    /// macOS: `rm images/data/name.img`.
    fn destroy_image_storage(name: &str) -> Result<()>;

    /// Mount a disk image and return the mount point path.
    ///
    /// Linux: loop-mounts the zvol block device.
    /// macOS: `hdiutil attach` the raw `.img` file.
    fn mount(path: &Path) -> Result<PathBuf>;

    /// Unmount a previously mounted disk image.
    ///
    /// Linux: `umount`.
    /// macOS: `hdiutil detach`.
    fn unmount(mount_point: &Path) -> Result<()>;
}

/// Network backend: manages VM networking.
///
/// - **Linux**: TAP devices + iptables NAT/masquerade + static IP allocation.
/// - **macOS**: vmnet shared mode (NAT + DHCP handled by the framework).
///
/// All methods are associated functions selected at compile time.
pub trait NetworkBackend {
    /// Set up networking for a VM. Returns the network configuration.
    ///
    /// Linux: allocates IP, creates TAP device, adds iptables NAT rules.
    /// macOS: no-op (vmnet handles everything); returns vmnet gateway info.
    fn setup(vm: &VmMetadata, config: &GlobalConfig) -> Result<NetworkInfo>;

    /// Tear down networking for a VM.
    ///
    /// Linux: removes iptables rules, deletes TAP device, releases IP.
    /// macOS: no-op (vmnet cleans up automatically).
    fn teardown(vm: &VmMetadata) -> Result<()>;

    /// Discover the guest's IP address from its MAC address.
    ///
    /// Linux: the IP is statically assigned, so this is a lookup.
    /// macOS: parses `/var/db/dhcpd_leases` for the vmnet DHCP lease,
    /// with ARP-based fallback.
    fn discover_guest_ip(mac: &str) -> Result<String>;
}
