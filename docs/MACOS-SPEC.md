# Ember macOS Support Spec

This document specifies how ember provides the same CLI experience on macOS by substituting platform-appropriate backends for each Linux-specific subsystem.

## Design Principles

- **Same CLI, different backends**: All `ember` commands (`init`, `vm create/start/stop`, `ssh`, `vm fork`, etc.) work identically on macOS. The platform difference is invisible to users.
- **No root required**: Unlike Linux (where TAP devices, iptables, and ZFS all require root), the macOS backend runs entirely without `sudo`.
- **Native tools**: Use Apple's own frameworks (Virtualization.framework, vmnet, APFS) rather than porting Linux tools. This matches ember's philosophy of shelling out to platform tools.
- **Minimal external dependencies**: Only Homebrew packages that aren't avoidable (`e2fsprogs` for ext4, `skopeo` for OCI pulls).

## Component Mapping

| Linux | macOS | Notes |
|-------|-------|-------|
| Firecracker (KVM) | Apple Virtualization Framework (AVF) | Native hypervisor, macOS 13+ |
| ZFS zvols + clones | APFS clones (`cp -c`) + raw disk images | Zero-cost CoW clones |
| TAP devices (ioctl) | vmnet framework (shared mode) | Built-in NAT, static IP allocation |
| iptables (NAT/masquerade) | vmnet (handles NAT internally) | No manual firewall rules |
| `ip` command | Not needed | vmnet manages devices |
| `sysctl ip_forward` | Not needed | vmnet handles routing |
| `mount -o loop` | `debugfs -w` (e2fsprogs) | Inspect/modify ext4 images without mounting |
| `umount` | (not needed) | `debugfs` operates directly on image files |
| `/var/lib/ember/` | `~/Library/Application Support/ember/` | macOS convention |

## Virtualization: Apple Virtualization Framework

### Why AVF

- Native performance via Apple's Hypervisor.framework (no emulation overhead)
- Ships with macOS 13+ — no install required (Linux boot support requires 13+)
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
                              ├── VZNATNetworkDeviceAttachment (vmnet)
                              ├── VZVirtioEntropyDeviceConfiguration (/dev/urandom)
                              └── VZVirtioTraditionalMemoryBalloonDeviceConfiguration
```

`ember-vz` is a Swift Package Manager project compiled alongside ember. It exposes a CLI interface:

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
```

`ember-vz` validates CPU and memory values against AVF's allowed min/max bounds at startup.

The `--ready-fd` flag causes `ember-vz` to write the guest's vmnet-assigned MAC address to the given file descriptor once the VM is booted, allowing ember to discover the guest IP.

### VM Lifecycle

**Start sequence** (analogous to Firecracker start):
1. Load VM metadata
2. `ember-vz start` with kernel, disk image, CPU/memory config
3. Wait for ready signal on fd 3 (guest MAC address)
4. Guest IP is already known (statically allocated before boot)
5. Wait for SSH (same exponential backoff as Linux)

**Stop sequence:**
1. `kill -TERM <ember-vz-pid>` — triggers ACPI `requestStop()`
2. `ember-vz` internally escalates to `vm.stop()` after 5s if ACPI shutdown doesn't complete
3. Rust side waits up to 10s total for process exit
4. `kill -KILL` if still alive
5. Update state: Stopped

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
│       ├── vm.json                    # VM metadata (includes PID when running)
│       ├── rootfs.img                 # APFS clone of base image
│       └── console.log               # Serial console output
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
    │  (mkfs.ext4 -d <dir> via Homebrew e2fsprogs — single-step create+populate)
    ▼
Raw ext4 image file: ~/Library/Application Support/ember/images/data/<name>-<tag>.img
    │  (resize2fs -M + truncate — shrink to minimum size)
    ▼
Compact sparse image
```

No zvol, no `dd`, no `@base` snapshot. The raw `.img` file *is* the base image.

`mkfs.ext4 -d` creates and populates the filesystem in one step, avoiding the need to mount the ext4 image (macOS has no native ext4 mount support). If a `fakeroot.state` file exists from tar extraction, `mkfs.ext4` runs under `fakeroot -i` to preserve correct uid/gid ownership.

### VM Create (Instant APFS Clone)

```bash
cp -c images/data/<name>-<tag>.img vms/<vm-name>/rootfs.img
```

This is instant regardless of image size (APFS copy-on-write). The raw image file is passed directly to AVF as a virtio block device.

After cloning, per-VM SSH keys are injected using `debugfs -w` from Homebrew e2fsprogs. Since macOS cannot mount ext4 natively, `debugfs` writes directly to the image file without mounting:

1. `debugfs -R 'stat /home/<user>'` — detect SSH user and uid/gid
2. `debugfs -w -f <commands>` — create `.ssh/` directory, write `authorized_keys`, fix permissions/ownership via `set_inode_field`

### VM Fork

```bash
# Clone source disk into a new VM
cp -c vms/<source>/rootfs.img vms/<new-name>/rootfs.img
```

Same instant CoW semantics as ZFS clone. APFS reference-counts blocks internally, so source and fork share storage until they diverge.

### VM Resize

Since rootfs is a raw disk image file:

```bash
# Grow the file
truncate -s <new-size> vms/<vm-name>/rootfs.img

# Check and grow the filesystem (no mount needed)
e2fsck -f vms/<vm-name>/rootfs.img
resize2fs vms/<vm-name>/rootfs.img
```

### Comparison with ZFS

| Operation | ZFS (Linux) | APFS (macOS) |
|-----------|-------------|--------------|
| Base image | zvol + `@base` snapshot | Raw `.img` file |
| VM clone | `zfs clone pool/images/x@base pool/vms/y` | `cp -c images/x.img vms/y/rootfs.img` |
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
                  ──────────────────
Total logical:    28.8 GB
Actual disk used:  4.1 GB  (via df)
CoW efficiency:    7.0x space savings
```

**How it works:**

1. **Logical size**: Sum of all `.img` file sizes via `stat` (apparent file size)
2. **Actual disk usage**: Sum of `st_blocks * 512` for each `.img` file — this reports actually-allocated 512-byte blocks, which reflects CoW sharing (APFS clones share blocks, so `st_blocks` is lower than the logical size)
3. **CoW ratio**: Logical size divided by actual disk usage

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

- **Built-in NAT**: vmnet shared mode provides NAT for outbound traffic with zero configuration
- **No root required**: Shared mode networking works without `sudo`
- **No manual firewall rules**: No `pf` or `iptables` equivalent needed
- **Direct AVF integration**: `VZNATNetworkDeviceAttachment` connects directly to vmnet

### How It Works

In shared mode, vmnet creates a virtual network (`192.168.64.0/24`) with a gateway (192.168.64.1) that performs NAT for outbound traffic.

Guest IPs are **statically allocated** from the vmnet subnet using the same `/30` block allocator as Linux (tracked in `network/allocations.json`). The allocated IP is passed to the kernel via the `ip=` boot parameter, so the guest has connectivity immediately at boot — no DHCP dependency.

This avoids relying on vmnet's built-in DHCP server, which can be blocked by VPN kill switches (e.g., Mullvad, Tailscale) that filter traffic on the vmnet bridge interface.

### DNS

vmnet shared mode's DHCP advertises the gateway (192.168.64.1) as DNS server, but the gateway does not actually forward DNS queries. During image injection, a static `/etc/resolv.conf` with public DNS servers (8.8.8.8, 1.1.1.1) is written instead of the Linux-style symlink to `/proc/net/pnp`.

### IP Allocation

Like Linux, macOS uses `/30` block allocation from the vmnet subnet (`192.168.64.0/24`), tracked in `network/allocations.json`. This gives 64 concurrent VMs. The allocated guest IP is passed to the kernel via boot args: `ip=<guest>::192.168.64.1:255.255.255.0:<vmname>:eth0:off`.

### Per-VM Network Info

```rust
pub struct NetworkInfo {
    pub guest_ip: String,      // Statically allocated (e.g., "192.168.64.2")
    pub host_ip: String,       // vmnet gateway ("192.168.64.1")
    pub guest_mac: String,     // Assigned by AVF/vmnet at boot
    // No tap_device — vmnet handles the virtual interface
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
    fn clone_vm_storage(src_vm: &str, dst_vm: &str) -> Result<PathBuf>;  // For vm fork
    fn resize(vm_name: &str, new_size: ByteSize) -> Result<()>;
    fn destroy_vm_storage(vm_name: &str) -> Result<()>;
    fn destroy_image_storage(name: &str) -> Result<()>;
    fn inject_ssh_key(vm_name: &str, pubkey: &str) -> Result<()>;  // debugfs on macOS, mount on Linux
}

/// Network backend (TAP+iptables on Linux, vmnet on macOS)
pub trait NetworkBackend {
    fn setup(vm: &VmMetadata, config: &GlobalConfig) -> Result<NetworkInfo>;
    fn teardown(vm: &VmMetadata) -> Result<()>;
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
│   │   ├── storage.rs      # ZFS zvol/clone operations
│   │   ├── network.rs      # TAP + iptables + IP allocation
│   │   └── image.rs        # ext4 creation with loop mount
│   └── macos/
│       ├── mod.rs
│       ├── vm.rs           # ember-vz process management
│       ├── storage.rs      # APFS clone + raw image + debugfs SSH injection
│       ├── network.rs      # vmnet IP allocation
│       └── image.rs        # ext4 creation with mkfs.ext4 -d + shrink-to-fit
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
- **macOS 13+**: For Virtualization.framework Linux boot support

### Homebrew
- **`e2fsprogs`**: Provides `mkfs.ext4`, `resize2fs`, `e2fsck`, and `debugfs` for ext4 image creation, resizing, and SSH key injection
- **`skopeo`**: OCI image pulling (same as Linux)

Tool paths are resolved for both Apple Silicon (`/opt/homebrew/opt/e2fsprogs/sbin/`) and Intel (`/usr/local/opt/e2fsprogs/sbin/`), with fallback to `$PATH`.

### Build
- `make build` orchestrates both Rust and Swift builds
- `cargo build` compiles the Rust CLI
- `swift build` compiles `ember-vz`
- `codesign` applies virtualization entitlement to `ember-vz`
- Both binaries are placed side-by-side in `target/{debug,release}/`

## Differences from Linux

| Aspect | Linux | macOS |
|--------|-------|-------|
| Root required | Yes | No |
| `ember init` | Creates ZFS pool + datasets | Creates directories only |
| VM boot console | `console=ttyS0` | `console=hvc0` |
| Disk device in guest | `/dev/vda` (virtio) | `/dev/vda` (virtio) |
| Network config | Static IP via kernel `ip=` param | Static IP via kernel `ip=` param |
| Guest IP | Known at start time (allocated) | Known at start time (allocated) |
| Kernel preset | Firecracker CI kernel | AVF-compatible kernel |
| Hypervisor process | `firecracker` (external binary) | `ember-vz` (bundled Swift binary) |
| State directory | `/var/lib/ember/` | `~/Library/Application Support/ember/` |
| Reconciliation | Check PID alive, cleanup TAP+iptables | Check PID alive (no network cleanup needed) |
| SSH key injection | Mount ext4 via loop device | `debugfs -w` (no mount needed) |
| ext4 image creation | `mkfs.ext4` + loop mount + `cp -a` | `mkfs.ext4 -d` (single step, no mount) |
| Image shrinking | Not needed (zvol sized) | `resize2fs -M` + `truncate` (minimize sparse image) |
