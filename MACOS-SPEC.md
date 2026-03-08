# Ember macOS Support Spec

This document specifies how ember provides the same CLI experience on macOS by substituting platform-appropriate backends for each Linux-specific subsystem.

## Design Principles

- **Same CLI, different backends**: All `ember` commands (`init`, `vm create/start/stop`, `ssh`, `snapshot`, etc.) work identically on macOS. The platform difference is invisible to users.
- **No root required**: Unlike Linux (where TAP devices, iptables, and ZFS all require root), the macOS backend runs entirely without `sudo`.
- **Native tools**: Use Apple's own frameworks (Virtualization.framework, vmnet, APFS) rather than porting Linux tools. This matches ember's philosophy of shelling out to platform tools.
- **Minimal external dependencies**: Only Homebrew packages that aren't avoidable (`e2fsprogs` for ext4, `skopeo` for OCI pulls).

## Component Mapping

| Linux | macOS | Notes |
|-------|-------|-------|
| Firecracker (KVM) | Apple Virtualization Framework (AVF) | Native hypervisor, macOS 12+ |
| ZFS zvols + snapshots | APFS clones (`cp -c`) + raw disk images | Zero-cost CoW clones |
| TAP devices (ioctl) | vmnet framework (shared mode) | Built-in NAT + DHCP |
| iptables (NAT/masquerade) | vmnet (handles NAT internally) | No manual firewall rules |
| `ip` command | Not needed | vmnet manages devices |
| `sysctl ip_forward` | Not needed | vmnet handles routing |
| `mount -o loop` | `hdiutil attach` | Mount raw disk images |
| `umount` | `hdiutil detach` | Unmount disk images |
| `/var/lib/ember/` | `~/Library/Application Support/ember/` | macOS convention |

## Virtualization: Apple Virtualization Framework

### Why AVF

- Native performance via Apple's Hypervisor.framework (no emulation overhead)
- Ships with macOS 12+ — no install required
- First-class Apple Silicon support with Rosetta 2 for x86 Linux guests
- Direct vmnet integration for networking
- Supports booting Linux kernels directly (like Firecracker)

### Architecture: Swift Helper Binary (`ember-vz`)

Rather than using Rust ObjC FFI (complex, fragile), ember shells out to a small Swift CLI tool called `ember-vz`. This matches ember's existing pattern of shelling out to `zfs`, `iptables`, etc.

```
ember (Rust) ──shells out──▶ ember-vz (Swift)
                              │
                              ├── VZVirtualMachine
                              ├── VZLinuxBootLoader
                              ├── VZVirtioBlockDeviceConfiguration
                              ├── VZVirtioNetworkDeviceConfiguration
                              └── VZNATNetworkDeviceAttachment (vmnet)
```

`ember-vz` is a Swift Package Manager project compiled alongside ember. It exposes a JSON-based CLI interface:

```bash
# Start a VM (blocks until VM exits or receives stop signal)
ember-vz start \
  --kernel /path/to/vmlinux \
  --disk /path/to/rootfs.img \
  --cpus 2 \
  --memory 512 \
  --boot-args "console=hvc0 root=/dev/vda rw" \
  --network shared \
  --serial-log /path/to/console.log \
  --ready-fd 3

# Stop a VM (sends signal to running ember-vz process)
kill -TERM <ember-vz-pid>

# Query VM status
ember-vz status --pid <pid>
```

The `--ready-fd` flag causes `ember-vz` to write the guest's vmnet-assigned MAC address to the given file descriptor once the VM is booted, allowing ember to discover the guest IP.

### VM Lifecycle

**Start sequence** (analogous to Firecracker start):
1. Load VM metadata
2. `ember-vz start` with kernel, disk image, CPU/memory config
3. Wait for ready signal on fd 3 (guest MAC address)
4. Discover guest IP from vmnet DHCP leases
5. Wait for SSH (same exponential backoff as Linux)

**Stop sequence:**
1. `kill -TERM <ember-vz-pid>` — triggers graceful ACPI shutdown
2. Wait up to 10s for process exit
3. `kill -KILL` if still alive
4. Update state: Stopped

**Pause/Resume:**
- AVF supports `pause()` and `resume()` on `VZVirtualMachine`
- `ember-vz` listens for `SIGUSR1` (pause) and `SIGUSR2` (resume)

### Kernel

AVF's `VZLinuxBootLoader` boots a `vmlinux` kernel directly, just like Firecracker. The same kernel presets work, though a separate macOS-compatible preset may be needed (Firecracker's kernel config is very minimal and may lack virtio drivers AVF needs).

Kernel preset for macOS:

| Preset | Description | Notes |
|--------|-------------|-------|
| `stock` | AVF-compatible Linux kernel | Must include virtio-blk, virtio-net, virtio-console drivers |

The stock kernel URL will differ between Linux (Firecracker CI kernel) and macOS (AVF-compatible kernel). The `kernel.rs` module selects the right preset based on `#[cfg(target_os)]`.

### Serial Console

AVF provides a virtio console device. `ember-vz` captures serial output to a log file, just like Firecracker's `console.log`. The guest boot args use `console=hvc0` instead of `console=ttyS0`.

## Storage: APFS Clones

### Why APFS Clones

- **Instant CoW clones**: `cp -c` creates a zero-cost copy-on-write clone, exactly like `zfs clone`
- **Native to macOS**: APFS is the default filesystem since macOS 10.13 (High Sierra)
- **No setup required**: Unlike ZFS (which needs `ember init` to create a pool), APFS just works
- **No root required**: Regular file operations, no special privileges

### Storage Layout

```
~/Library/Application Support/ember/
├── config.json
├── kernels/
│   └── vmlinux-avf                    # macOS kernel preset
├── images/
│   ├── registry.json
│   └── data/
│       └── <name>-<tag>.img           # Base ext4 disk image (raw)
├── vms/
│   └── <vm-name>/
│       ├── vm.json                    # VM metadata
│       ├── rootfs.img                 # APFS clone of base image
│       ├── ember-vz.pid              # Helper process PID
│       ├── console.log               # Serial console output
│       └── snapshots/
│           ├── snap1.img             # APFS clone at snapshot time
│           └── snap2.img
└── network/
    └── allocations.json              # Not needed for vmnet shared mode, but kept for consistency
```

### Image Pull Workflow

```
OCI registry
    │  (skopeo copy + tar extract layers)
    ▼
Unpacked rootfs directory
    │  (inject SSH authorized_keys, resolv.conf, inittab)
    ▼
Prepared rootfs
    │  (mkfs.ext4 via Homebrew e2fsprogs + hdiutil attach + cp -a)
    ▼
Raw ext4 image file: ~/Library/Application Support/ember/images/data/<name>-<tag>.img
```

No zvol, no `dd`, no `@base` snapshot. The raw `.img` file *is* the base image.

### VM Create (Instant APFS Clone)

```bash
cp -c images/data/<name>-<tag>.img vms/<vm-name>/rootfs.img
```

This is instant regardless of image size (APFS copy-on-write). The raw image file is passed directly to AVF as a virtio block device.

After cloning, the image is mounted via `hdiutil attach` to inject per-VM SSH keys, then detached.

### Snapshots

```bash
# Create: clone current state
cp -c vms/<vm-name>/rootfs.img vms/<vm-name>/snapshots/<snap-name>.img

# Restore: replace current with snapshot clone
cp -c vms/<vm-name>/snapshots/<snap-name>.img vms/<vm-name>/rootfs.img

# Delete: just remove the file
rm vms/<vm-name>/snapshots/<snap-name>.img
```

APFS handles the CoW reference counting internally. Deleting a snapshot only frees blocks not referenced by other clones.

### VM Fork

```bash
# Snapshot source, then clone
cp -c vms/<source>/rootfs.img vms/<new-name>/rootfs.img
```

Same instant CoW semantics as ZFS clone.

### VM Resize

Since rootfs is a raw disk image file:

```bash
# Grow the file
truncate -s <new-size> vms/<vm-name>/rootfs.img

# Grow the filesystem
# Mount via hdiutil, run resize2fs (from Homebrew e2fsprogs), detach
```

### Comparison with ZFS

| Operation | ZFS (Linux) | APFS (macOS) |
|-----------|-------------|--------------|
| Base image | zvol + `@base` snapshot | Raw `.img` file |
| VM clone | `zfs clone pool/images/x@base pool/vms/y` | `cp -c images/x.img vms/y/rootfs.img` |
| Snapshot | `zfs snapshot pool/vms/y@snap` | `cp -c vms/y/rootfs.img vms/y/snapshots/snap.img` |
| Restore | `zfs rollback pool/vms/y@snap` | `cp -c vms/y/snapshots/snap.img vms/y/rootfs.img` |
| Delete snap | `zfs destroy pool/vms/y@snap` | `rm vms/y/snapshots/snap.img` |
| Resize | `zfs set volsize=XG` + `resize2fs` | `truncate -s XG` + `resize2fs` |
| Fork | `zfs clone pool/vms/a@fork-b pool/vms/b` | `cp -c vms/a/rootfs.img vms/b/rootfs.img` |

## Verifying CoW Storage Efficiency

### The Problem

Unlike ZFS (where `zfs list -o used,refer` clearly shows per-dataset space usage and CoW savings), APFS has no per-file way to measure clone savings. Both `du` and Finder report clones as if they occupy full space. This means a user with 10 VMs cloned from a 2GB image would see `du` report 20GB even though actual disk usage is ~2GB.

### `ember debug storage-efficiency`

A built-in diagnostic command that reports CoW savings:

```
$ ember debug storage-efficiency

Storage Efficiency Report
─────────────────────────
Images:        2 (3.2 GB logical)
VMs:           8 (25.6 GB logical)
Snapshots:    12 (38.4 GB logical)
                  ──────────────────
Total logical:    67.2 GB
Actual disk used:  4.1 GB  (via df)
CoW efficiency:   16.4x space savings
```

**How it works:**

1. **Logical size**: Sum of all `.img` file sizes via `stat` (what `du` would report)
2. **Actual disk usage**: Measure free space on the APFS volume via `df` or `diskutil apfs list`, subtract from total capacity. Compare with a baseline taken during `ember init` or by subtracting non-ember usage
3. **Alternative**: Use `diskutil apfs listVolumeGroups` to get the container-level "Used" metric before and after operations

### `cp -c` Failure Detection

`cp -c` **fails with an error** rather than silently falling back to a full copy when CoW isn't possible:
- Cross-volume copy: `"clonefile failed: Cross-device link"`
- Non-APFS filesystem: `"clonefile failed: Not supported"`

Ember catches these errors and reports a clear message:

```
Error: VM storage must be on an APFS volume.
The state directory ~/Library/Application Support/ember/ is on a non-APFS
filesystem, which doesn't support copy-on-write clones.
```

### `ember init` APFS Validation

During `ember init` on macOS, verify that the state directory resides on an APFS volume:

```bash
diskutil info -plist "$(df /path/to/state-dir | tail -1 | awk '{print $1}')"
# Check FilesystemType == "apfs"
```

If not APFS, warn the user that cloning will be slow and use full disk space.

### Timing-Based Sanity Check

As an additional safeguard, `ember vm create` measures the wall-clock time of the `cp -c` operation. A CoW clone completes in milliseconds regardless of file size. If the clone takes longer than 1 second for a multi-GB image, log a warning:

```
Warning: disk clone took 3.2s — this may indicate copy-on-write is not working.
Run `ember debug storage-efficiency` to check.
```

## Networking: vmnet (Shared Mode)

### Why vmnet

- **Built-in NAT + DHCP**: vmnet shared mode provides a complete network stack — NAT, DHCP, DNS forwarding — with zero configuration
- **No root required**: Shared mode networking works without `sudo`
- **No manual firewall rules**: No `pf` or `iptables` equivalent needed
- **Direct AVF integration**: `VZNATNetworkDeviceAttachment` connects directly to vmnet

### How It Works

In shared mode, vmnet creates a virtual network (typically `192.168.64.0/24`) with:
- A gateway that performs NAT for outbound traffic
- A DHCP server that assigns IPs to guests
- DNS forwarding to the host's configured DNS servers

The guest boots, gets a DHCP lease, and can immediately access the internet. No kernel `ip=` parameter needed for networking (though it can still be used for static IP if preferred).

### Guest IP Discovery

Since vmnet assigns IPs via DHCP, ember needs to discover the guest's IP after boot:

1. **Primary**: Parse vmnet DHCP lease file (`/var/db/dhcpd_leases`) — match by MAC address (reported by `ember-vz` via ready-fd)
2. **Fallback**: ARP scan the vmnet subnet for the known MAC address
3. **Last resort**: Try SSH on all IPs in the vmnet range

The discovered IP is stored in `vm.json` just like on Linux.

### DNS

vmnet shared mode forwards DNS automatically. No special resolv.conf injection needed — the DHCP lease includes DNS server information. However, the resolv.conf symlink to `/proc/net/pnp` is still injected for consistency with the Linux path (the kernel `ip=` parameter can optionally include DNS servers).

### No IP Allocation State

Unlike Linux (which tracks /30 allocations in `allocations.json`), macOS delegates IP allocation entirely to vmnet's DHCP. The `network/allocations.json` file is not used on macOS.

### Per-VM Network Info

```rust
pub struct NetworkInfo {
    pub guest_ip: String,      // DHCP-assigned (e.g., "192.168.64.3")
    pub host_ip: String,       // vmnet gateway (e.g., "192.168.64.1")
    pub guest_mac: String,     // Assigned by AVF/vmnet
    // No tap_device, no subnet allocation — vmnet handles it
}
```

## `ember init` on macOS

On macOS, `ember init` is much simpler — no ZFS pool creation needed:

1. Create state directory (`~/Library/Application Support/ember/`)
2. Create subdirectories: `kernels/`, `images/data/`, `vms/`, `network/`
3. Download macOS kernel preset if needed
4. Detect WAN interface (`route get 8.8.8.8` instead of `ip route get 8.8.8.8`)
5. Write `config.json`

No `--pool` or `--device` flags on macOS (they're Linux-only for ZFS setup).

## Code Architecture

### Backend Traits

```rust
/// Hypervisor backend (Firecracker on Linux, AVF on macOS)
pub trait VmBackend {
    fn start(vm: &VmMetadata, config: &GlobalConfig) -> Result<StartedVm>;
    fn stop(vm: &VmMetadata) -> Result<()>;
    fn force_stop(vm: &VmMetadata) -> Result<()>;
    fn pause(vm: &VmMetadata) -> Result<()>;
    fn resume(vm: &VmMetadata) -> Result<()>;
    fn is_running(pid: u32) -> bool;
}

/// Storage backend (ZFS on Linux, APFS on macOS)
pub trait StorageBackend {
    fn init(config: &InitConfig) -> Result<()>;
    fn create_image_volume(name: &str, image_path: &Path) -> Result<PathBuf>;
    fn clone_for_vm(image_name: &str, vm_name: &str) -> Result<PathBuf>;
    fn snapshot(vm_name: &str, snap_name: &str) -> Result<()>;
    fn restore_snapshot(vm_name: &str, snap_name: &str) -> Result<()>;
    fn delete_snapshot(vm_name: &str, snap_name: &str) -> Result<()>;
    fn list_snapshots(vm_name: &str) -> Result<Vec<SnapshotInfo>>;
    fn resize(vm_name: &str, new_size: ByteSize) -> Result<()>;
    fn destroy_vm_storage(vm_name: &str) -> Result<()>;
    fn destroy_image_storage(name: &str) -> Result<()>;
    fn mount(path: &Path) -> Result<PathBuf>;   // Returns mount point
    fn unmount(mount_point: &Path) -> Result<()>;
}

/// Network backend (TAP+iptables on Linux, vmnet on macOS)
pub trait NetworkBackend {
    fn setup(vm: &VmMetadata, config: &GlobalConfig) -> Result<NetworkInfo>;
    fn teardown(vm: &VmMetadata) -> Result<()>;
    fn discover_guest_ip(mac: &str) -> Result<String>;
}
```

### Module Structure

```
src/
├── backend/
│   ├── mod.rs              # Trait definitions + #[cfg] re-exports
│   ├── linux/
│   │   ├── mod.rs
│   │   ├── vm.rs           # Firecracker process management + API
│   │   ├── storage.rs      # ZFS zvol/snapshot/clone operations
│   │   ├── network.rs      # TAP + iptables + IP allocation
│   │   └── image.rs        # ext4 creation with loop mount
│   └── macos/
│       ├── mod.rs
│       ├── vm.rs           # ember-vz process management
│       ├── storage.rs      # APFS clone + raw image operations
│       ├── network.rs      # vmnet IP discovery
│       └── image.rs        # ext4 creation with hdiutil
├── cli/                    # Unchanged — calls backend traits
├── ssh/                    # Unchanged — russh is cross-platform
├── state/                  # Unchanged — JSON + flock works on macOS
├── config/                 # Unchanged — YAML parsing
├── image/                  # Mostly unchanged — skopeo + tar + inject
├── kernel.rs               # Platform-specific preset URLs
├── cleanup.rs              # Unchanged — RAII pattern
└── error.rs                # Unchanged
```

### Compile-Time Selection

```rust
// src/backend/mod.rs
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;
```

## External Dependencies on macOS

### Required
- **Xcode Command Line Tools**: For compiling the Swift helper (`ember-vz`)
- **macOS 12+**: For Virtualization.framework

### Homebrew
- **`e2fsprogs`**: Provides `mkfs.ext4` and `resize2fs` for ext4 image creation
- **`skopeo`**: OCI image pulling (same as Linux)

### Build
- `cargo build` compiles the Rust CLI
- `swift build` compiles `ember-vz` (or integrated into cargo build via build script)
- Both binaries are distributed together

## Differences from Linux

| Aspect | Linux | macOS |
|--------|-------|-------|
| Root required | Yes | No |
| `ember init` | Creates ZFS pool + datasets | Creates directories only |
| VM boot console | `console=ttyS0` | `console=hvc0` |
| Disk device in guest | `/dev/vda` (virtio) | `/dev/vda` (virtio) |
| Network config | Static IP via kernel `ip=` param | DHCP via vmnet |
| Guest IP | Known at start time (allocated) | Discovered after boot (DHCP) |
| Kernel preset | Firecracker CI kernel | AVF-compatible kernel |
| Hypervisor process | `firecracker` (external binary) | `ember-vz` (bundled Swift binary) |
| State directory | `/var/lib/ember/` | `~/Library/Application Support/ember/` |
| Reconciliation | Check PID alive, cleanup TAP+iptables | Check PID alive (no network cleanup needed) |
