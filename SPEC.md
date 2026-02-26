# Crackling — Lightweight Firecracker VM Manager

A CLI tool for managing Firecracker microVMs with ZFS-backed storage. CLI-only — no daemon, no REST API.

## Design Principles

- **CLI-first**: All operations via command line. No background daemon.
- **ZFS-native**: ZFS zvols as block devices for VMs. Instant cloning from snapshots. User-facing snapshot operations.
- **Minimal moving parts**: Shell out to `zfs`/`zpool`/`iptables` CLI tools rather than fragile library bindings. Thin custom Firecracker API client over Unix socket.
- **Root required**: TAP devices, iptables, ZFS, loop mounting, and Firecracker all need root. Like Docker — run as root.

## CLI Commands

```
crackling
├── init [--pool <name>] [--device <path>] [--dataset <name>] [--kernel-url <url>]
│
├── vm
│   ├── create <name> --image <image> [--cpus N] [--memory MiB] [--disk-size GiB]
│   │          [--kernel <path>] [--network <subnet>] [--config <file>] [--no-start]
│   ├── start <name>
│   ├── stop <name> [--force]
│   ├── pause <name>
│   ├── resume <name>
│   ├── delete <name> [--force]
│   ├── list [--format table|json]
│   ├── inspect <name> [--format json]
│   └── ssh <name> [-- <command>...]
│
├── image
│   ├── pull <reference>           # e.g. docker.io/library/ubuntu:22.04
│   ├── list [--format table|json]
│   ├── delete <name>
│   └── inspect <name>
│
├── snapshot
│   ├── create <vm-name> <snapshot-name>
│   ├── restore <vm-name> <snapshot-name>
│   ├── list <vm-name> [--format table|json]
│   └── delete <vm-name> <snapshot-name>
│
├── exec <vm-name> [--user <user>] -- <command>...
│
├── cp <src> <dst>                 # prefix with <vm-name>: for remote paths
│
└── version
```

### Global Flags

```
--state-dir <path>     # Override state directory (default: /var/lib/crackling)
--log-level <level>    # trace, debug, info, warn, error (default: info)
--config <file>        # Global config file override
```

### YAML Config (for `vm create --config`)

```yaml
name: myvm
image: docker.io/library/ubuntu:22.04
cpus: 2
memory: 512          # MiB
disk_size: 4         # GiB
kernel: /path/to/custom/vmlinux  # optional
network:
  subnet: 10.100.0.0/16
ssh:
  user: root
  key: ~/.ssh/id_ed25519
boot_args: "console=ttyS0 reboot=k panic=1 pci=off"
```

Merge order: defaults < global config < per-VM YAML < CLI flags.

## Architecture

```
src/
├── main.rs              # Entry point, CLI dispatch
├── cli/
│   ├── mod.rs           # clap App definition
│   ├── init.rs          # crackling init
│   ├── vm.rs            # crackling vm *
│   ├── image.rs         # crackling image *
│   ├── snapshot.rs      # crackling snapshot *
│   ├── exec.rs          # crackling exec
│   └── cp.rs            # crackling cp
├── zfs/
│   ├── mod.rs
│   ├── pool.rs          # zpool create/status
│   ├── dataset.rs       # zfs create/destroy/list
│   ├── volume.rs        # zvol operations (block devices)
│   └── snapshot.rs      # zfs snapshot/rollback/clone/destroy
├── firecracker/
│   ├── mod.rs
│   ├── api.rs           # HTTP-over-Unix-socket client (hyper + hyperlocal)
│   ├── config.rs        # VM config builder → API call sequence
│   └── process.rs       # Spawn/wait/kill firecracker process
├── network/
│   ├── mod.rs
│   ├── tap.rs           # TAP device via ioctl (nix crate)
│   ├── ip.rs            # IP allocation from pool
│   └── nat.rs           # iptables NAT/masquerade rules
├── image/
│   ├── mod.rs
│   ├── pull.rs          # OCI image pull (oci-unpack or skopeo fallback)
│   ├── unpack.rs        # Layer extraction
│   ├── ext4.rs          # mkfs.ext4 + loop mount + rootfs copy
│   └── registry.rs      # Local image metadata
├── ssh/
│   ├── mod.rs
│   ├── client.rs        # SSH connection (russh)
│   ├── exec.rs          # Remote command execution
│   └── copy.rs          # SCP file transfer
├── state/
│   ├── mod.rs
│   ├── store.rs         # JSON files + flock
│   └── vm.rs            # VM metadata types
└── config/
    ├── mod.rs
    └── vm.rs            # YAML config parsing + merge
```

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| clap (derive) | CLI parsing |
| serde, serde_json, serde_yaml | Config and state serialization |
| tokio | Async runtime |
| hyper + hyperlocal | HTTP over Unix socket (Firecracker API) |
| nix | TAP device ioctl, process signals |
| russh + russh-keys | SSH client |
| thiserror, anyhow | Error handling |
| uuid | VM identifiers |
| indicatif | Progress bars for image pulls |

**No ZFS crate** — shell out to `zfs`/`zpool` CLI. The Rust ZFS crates are unmaintained or FreeBSD-only. Shelling out is standard practice (Proxmox, TrueNAS).

**No Firecracker SDK crate** — the API is ~10 REST endpoints. A custom thin client with hyper is ~200 lines and avoids version coupling.

## Storage: ZFS

### Dataset Layout

```
<pool>/
├── images/
│   └── <name>-<tag>          # zvol, block device for base image
│       └── @base             # snapshot, cloned per VM
└── vms/
    └── <vm-name>             # zvol, cloned from image snapshot
        ├── @snap1            # user snapshots
        └── @snap2
```

### Image Pull Workflow

```
OCI registry
    │  (oci-unpack or skopeo)
    ▼
Unpacked layer directory (/tmp/crackling-image-XXXX/)
    │  (mkfs.ext4 + loop mount + copy)
    ▼
ext4 image file
    │  (dd to zvol)
    ▼
ZFS zvol: <pool>/images/<name>-<tag>
    │  (zfs snapshot)
    ▼
ZFS snapshot: <pool>/images/<name>-<tag>@base
```

### VM Create (Instant Clone)

```
zfs clone <pool>/images/<name>-<tag>@base <pool>/vms/<vm-name>
```

This is instant regardless of image size (copy-on-write). The zvol appears as `/dev/zvol/<pool>/vms/<vm-name>` — passed directly to Firecracker as the root drive block device.

### User Snapshots

```
crackling snapshot create myvm snap1   →  zfs snapshot <pool>/vms/myvm@snap1
crackling snapshot restore myvm snap1  →  zfs rollback <pool>/vms/myvm@snap1  (VM must be stopped)
crackling snapshot list myvm           →  zfs list -t snapshot -r <pool>/vms/myvm
crackling snapshot delete myvm snap1   →  zfs destroy <pool>/vms/myvm@snap1
```

## Firecracker Integration

### VM Start Sequence

1. Load VM metadata from state store
2. Create TAP device + allocate IP
3. Configure iptables NAT rules
4. Spawn: `firecracker --api-sock <sock-path> --log-path <log-path> --level Info`
5. Wait for API socket (poll 10ms, timeout 5s)
6. Configure via API:
   - `PUT /machine-config` — vcpu_count, mem_size_mib
   - `PUT /boot-source` — kernel_image_path, boot_args (including `ip=` param)
   - `PUT /drives/rootfs` — path_on_host: `/dev/zvol/...`, is_root_device: true
   - `PUT /network-interfaces/eth0` — host_dev_name: TAP device, guest_mac
7. `PUT /actions { action_type: "InstanceStart" }`
8. Update state: Running + PID
9. Wait for SSH to become available (exponential backoff, ~30s timeout)

### VM Stop Sequence

1. `PUT /actions { action_type: "SendCtrlAltDel" }`
2. Wait up to 10s for process exit
3. SIGKILL if still alive
4. Cleanup: remove TAP, remove iptables rules, release IP
5. Update state: Stopped

### Pause/Resume

- Pause: `PATCH /vm { state: "Paused" }`
- Resume: `PATCH /vm { state: "Resumed" }`

### Boot Arguments

```
console=ttyS0 reboot=k panic=1 pci=off ip=<guest-ip>::<gateway>:<netmask>::eth0:off
```

The kernel `ip=` parameter configures guest networking at boot. No cloud-init or DHCP needed.

## Networking

### Model: TAP + NAT per VM

Each VM gets an isolated point-to-point link:

```
Host: cr-<short-id> (TAP)  10.100.0.1/30  ←→  Guest: eth0  10.100.0.2/30
```

### IP Allocation

- Configurable base subnet (default: `10.100.0.0/16`)
- Sequential /30 blocks: `10.100.0.0/30`, `10.100.0.4/30`, `10.100.0.8/30`, ...
- Host gets .1, guest gets .2 in each /30
- Supports ~16384 concurrent VMs with a /16
- Allocations tracked in state store, released on VM delete

### Setup (per VM start)

1. Create TAP device via ioctl (`/dev/net/tun`, IFF_TAP | IFF_NO_PI)
2. `ip addr add <host-ip>/30 dev cr-<short-id>` + `ip link set up`
3. Enable IP forwarding: `sysctl net.ipv4.ip_forward=1`
4. iptables rules:
   ```
   -t nat -A POSTROUTING -s <guest-ip>/32 -o <wan-iface> -j MASQUERADE
   -A FORWARD -i <tap-dev> -o <wan-iface> -j ACCEPT
   -A FORWARD -i <wan-iface> -o <tap-dev> -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
   ```

### Cleanup (per VM stop/delete)

1. `iptables -D` (same rules with delete flag)
2. `ip link delete cr-<short-id>`
3. Release IP allocation

### WAN Interface Detection

`ip route get 8.8.8.8 | grep -oP 'dev \K\S+'` — cached at init, overridable via config.

## State Management

### State Directory (`/var/lib/crackling/`)

```
/var/lib/crackling/
├── config.json
├── kernels/
│   └── vmlinux-<version>
├── images/
│   └── registry.json
├── vms/
│   └── <vm-name>/
│       ├── vm.json
│       ├── firecracker.sock
│       ├── firecracker.log
│       └── firecracker.pid
└── network/
    └── allocations.json
```

### VM Metadata (`vm.json`)

```rust
pub struct VmMetadata {
    pub name: String,
    pub id: Uuid,
    pub status: VmStatus,        // Created, Running, Stopped, Paused
    pub image: String,
    pub cpus: u32,
    pub memory_mib: u32,
    pub disk_size_gib: u32,
    pub kernel_path: PathBuf,
    pub zvol_path: String,
    pub network: Option<NetworkConfig>,
    pub pid: Option<u32>,
    pub api_socket: PathBuf,
    pub created_at: String,
    pub ssh: SshConfig,
}
```

### Concurrency

- Per-VM files: independent, no contention
- Shared files (allocations.json, registry.json): `flock(LOCK_EX)` on write, `flock(LOCK_SH)` on read
- Atomic writes: write to temp file, then `rename()` to final path

### Crash Recovery

On every command invocation, lightweight reconciliation:
- For each VM in Running state, check if PID is alive (`kill(pid, 0)`)
- Dead process → mark Stopped, cleanup TAP + iptables
- Orphaned `cr-*` TAP devices without running VM → delete

### Cleanup on Delete

1. Stop if running (or `--force` → SIGKILL)
2. Remove iptables rules
3. Delete TAP device
4. Release IP allocation
5. `zfs destroy` zvol (and snapshots under it)
6. Remove state directory

Each step is idempotent — continues if resource already gone.

## Guest Access (SSH-based)

No custom guest agent initially. All guest interaction over SSH:

- **exec**: Open SSH channel, run command, stream stdout/stderr, return exit code
- **cp**: SCP-style file transfer (both directions, detected by `<vm-name>:` prefix)
- **ssh**: Convenience wrapper for interactive SSH session

SSH readiness: exponential backoff retry after VM boot, up to ~30s timeout.

Authentication: SSH key from config (default `~/.ssh/id_ed25519`), injected into rootfs at image pull time.

Future: custom Rust agent over virtio-vsock for exec/cp without requiring SSH.
