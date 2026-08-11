# Ember — Lightweight Firecracker VM Manager

A CLI tool for managing Firecracker microVMs with ZFS-backed storage. CLI-only — no daemon, no REST API.

> **macOS**: Ember also runs on macOS using Apple Virtualization Framework + APFS clones instead of Firecracker + ZFS. See [MACOS-SPEC.md](MACOS-SPEC.md) for the macOS-specific design. This document covers the Linux backend.

## Design Principles

- **CLI-first**: All operations via command line. No background daemon.
- **ZFS-native**: ZFS zvols as block devices for VMs. Instant cloning from snapshots. Forks via CoW clone of a VM's disk.
- **Minimal moving parts**: Shell out to `zfs`/`zpool`/`iptables` CLI tools rather than fragile library bindings. Thin custom Firecracker API client over Unix socket.
- **Root required**: TAP devices, iptables, ZFS, loop mounting, and Firecracker all need root. Like Docker — run as root.

## CLI Commands

```
ember
├── init [--pool <name>] [--device <path>] [--dataset <name>] [--kernel <preset|path>]
│        [--wan-iface <iface>]
│
├── vm
│   ├── create <name> --image <image> [--cpus N] [--memory SIZE] [--disk-size SIZE]
│   │          [--kernel <preset|path>] [--network <subnet>] [--vm-config <file>] [--no-start]
│   ├── start <name>
│   ├── stop <name> [--force]
│   ├── pause <name>
│   ├── resume <name>
│   ├── resize <name> --disk-size <SIZE>
│   ├── delete <name> [--force]
│   ├── list [--format table|json]
│   ├── inspect <name> [--format table|json]
│   └── fork <source> <name> [--cpus N] [--memory SIZE] [--disk-size SIZE]
│              [--kernel <preset|path>] [--network <subnet>] [--no-start]
│
├── image
│   ├── pull <reference>           # e.g. docker.io/library/alpine:latest
│   ├── build <name> [-f|--file <dockerfile>]
│   ├── list [--format table|json]
│   ├── delete <name> [--force]
│   └── inspect <name> [--format table|json]
│
├── ssh <name> [-- <command>...]
│
├── exec <vm-name> [--user <user>] -- <command>...
│
├── cp <src> <dst>                 # prefix with <vm-name>: for remote paths
│
├── reconcile                      # manually trigger state reconciliation
│
└── version
```

### Global Flags

```
--state-dir <path>     # Override state directory (default: /var/lib/ember)
```

### YAML Config (for `vm create --vm-config`)

```yaml
name: myvm
image: ubuntu
cpus: 2
memory: 512M
disk_size: 4G
kernel: stock                    # preset name or file path (optional)
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
│   ├── init.rs          # ember init
│   ├── vm.rs            # ember vm *
│   ├── image.rs         # ember image *
│   ├── ssh.rs           # ember ssh
│   ├── exec.rs          # ember exec
│   └── cp.rs            # ember cp
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
│   ├── iptables.rs      # iptables invocation: rules, chains, locking
│   ├── nat.rs           # Per-VM masquerade and forwarding rules
│   ├── policy.rs        # Install-owned chains: VM-to-VM, host isolation
│   ├── dns.rs           # Host DNS nameserver detection for guests
│   └── wan.rs           # WAN interface auto-detection
├── image/
│   ├── mod.rs
│   ├── pull.rs          # OCI image pull via skopeo + layer extraction
│   ├── build.rs         # Dockerfile-based image building (docker/podman)
│   ├── ext4.rs          # mkfs.ext4 + loop mount + rootfs copy
│   ├── zvol.rs          # Write ext4 image to zvol + @base snapshot
│   ├── inject.rs        # SSH key, resolv.conf, inittab injection into rootfs
│   └── registry.rs      # Local image metadata
├── ssh/
│   ├── mod.rs
│   ├── client.rs        # SSH connection (russh)
│   ├── exec.rs          # Remote command execution
│   └── copy.rs          # SCP file transfer
├── state/
│   ├── mod.rs
│   ├── store.rs         # JSON files + flock
│   ├── vm.rs            # VM metadata types
│   └── reconcile.rs     # Crash recovery reconciliation
├── config/
│   ├── mod.rs
│   ├── vm.rs            # YAML config parsing + merge
│   └── size.rs          # ByteSize parsing (512M, 16G, etc.)
├── kernel.rs            # Named kernel presets (stock) + resolution
├── cleanup.rs           # RAII rollback guard for multi-step operations
└── error.rs             # Unified thiserror-based error types
```

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| clap (derive) | CLI parsing |
| serde, serde_json, serde_yaml | Config and state serialization |
| tokio | Async runtime |
| hyper + hyper-util + http-body-util + hyperlocal | HTTP over Unix socket (Firecracker API) |
| nix | TAP device ioctl, process signals |
| russh + russh-keys | SSH client |
| thiserror, anyhow | Error handling |
| uuid | VM identifiers |
| indicatif | Progress bars for image pulls |
| tempfile | Temporary directories for image build/pull pipelines |

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
        └── @fork-<child>     # snapshot, cloned per fork (one per child)
```

### Image Pull Workflow

```
OCI registry
    │  (skopeo copy + tar extract layers)
    ▼
Unpacked rootfs directory (/tmp/ember-image-XXXX/rootfs/)
    │  (inject SSH authorized_keys, resolv.conf, inittab)
    ▼
Prepared rootfs
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

### Image Build Workflow

```
ember image build <name> [-f|--file <dockerfile>]
```

Builds a VM image from a Dockerfile using Docker or Podman. If no Dockerfile is given, uses a built-in Ubuntu 26.04 VM image with systemd, sshd, and an `ubuntu` user with passwordless sudo.

```
Dockerfile
    │  (docker build + docker export)
    ▼
Exported rootfs tarball
    │  (tar extract)
    ▼
Unpacked rootfs directory
    │  (inject SSH authorized_keys, resolv.conf)
    ▼
Prepared rootfs
    │  (mkfs.ext4 + loop mount + copy)
    ▼
ext4 image file
    │  (dd to zvol)
    ▼
ZFS zvol: <pool>/images/<name>
    │  (zfs snapshot)
    ▼
ZFS snapshot: <pool>/images/<name>@base
```

Built images skip `inittab` injection because the default Dockerfile uses systemd, which handles init and CtrlAltDel natively.

### VM Create (Instant Clone + Per-VM SSH Key)

```
zfs clone <pool>/images/<name>-<tag>@base <pool>/vms/<vm-name>
```

This is instant regardless of image size (copy-on-write). The zvol appears as `/dev/zvol/<pool>/vms/<vm-name>` — passed directly to Firecracker as the root drive block device.

SSH key injection happens at two stages:

1. **Image pull/build time**: The invoking user's default SSH public key is injected into the rootfs *before* the `@base` snapshot is created, providing a working key in the base image.

2. **VM creation time**: After cloning, the VM's zvol is loop-mounted and the key is injected again into the per-VM clone. The target user is auto-detected: if `/home/ubuntu` exists in the rootfs, the key goes there and SSH connects as `ubuntu`; otherwise it targets `/root` and SSH connects as `root`.

This keeps the `@base` snapshot shareable across all VMs while giving each VM its own key. The SSH public key is auto-discovered from `~/.ssh/` in preference order: `id_ed25519.pub`, `id_ecdsa.pub`, `id_rsa.pub`. When running under `sudo`, the real user's home directory is resolved via `SUDO_USER`.

### VM Resize

```
ember vm resize myvm --disk-size 8G
```

1. VM must be stopped
2. `zfs set volsize=<size>G <pool>/vms/<vm-name>` — grows the zvol
3. Loop-mount the zvol, run `resize2fs` to expand ext4 to fill the new space
4. Update `disk_size_gib` in VM metadata

Shrinking is not supported — only growing. The command errors if the new size is smaller than or equal to the current size.

### VM Fork (Instant Clone)

```
ember vm fork <source> <name> [--cpus N] [--memory SIZE] [--disk-size SIZE]
                               [--kernel <preset|path>] [--network <subnet>] [--no-start]
```

Fork creates an independent copy-on-write clone of an existing VM. The source VM must be stopped.

1. Snapshot the source zvol: `zfs snapshot <pool>/vms/<source>@fork-<name>`
2. Clone the snapshot: `zfs clone <pool>/vms/<source>@fork-<name> <pool>/vms/<name>`
3. If `--disk-size` is larger than source, grow the zvol and `resize2fs`
4. Loop-mount the clone and re-inject the invoking user's SSH key
5. Start the forked VM (unless `--no-start`)

The fork inherits the source VM's configuration (cpus, memory, disk size, kernel, subnet, SSH config). CLI flags override inherited values. Disk can grow but not shrink below the source size.

The `forked_from` field in VM metadata tracks the origin snapshot path (e.g., `<pool>/vms/source@fork-newname`).

**Cleanup:**

- Deleting a forked VM destroys its zvol and the fork snapshot on the source.
- Deleting a source VM that still has fork snapshots (i.e., forks depend on it) is refused — the dependent VMs are listed. Use `--force` to cascade-delete the dependent VMs first.

## Firecracker Integration

### VM Start Sequence

1. Load VM metadata from state store
2. Create TAP device + allocate IP
3. Assert the installation's firewall chains, add the VM's iptables rules
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
console=ttyS0 reboot=k panic=1 pci=off ip=<guest-ip>::<gateway>:<netmask>:<hostname>:eth0:off:<dns0>:<dns1>
```

The kernel `ip=` parameter configures guest networking at boot. No cloud-init or DHCP needed. The VM name is passed as `<hostname>` so the kernel sets it at boot. DNS servers are appended to the `ip=` parameter — the kernel writes them to `/proc/net/pnp`, which the guest symlinks as `/etc/resolv.conf` (see "Guest DNS" below). At most 2 servers are included (kernel limit).

### Kernel Presets

The `--kernel` flag on `ember init` and `ember vm create` accepts either a named preset or a file path. Presets are auto-downloaded to `<state-dir>/kernels/` on first use.

| Preset | Description | Kernel |
|--------|-------------|--------|
| `stock` | Firecracker CI kernel (default). Includes overlayfs, cgroups, namespaces, iptables, bridge, veth, and virtio-rng. | vmlinux-6.1.102 |

Examples:
```
ember init --kernel stock
ember vm create myvm --image alpine:latest --kernel /path/to/custom/vmlinux
```

YAML configs also accept preset names: `kernel: stock`.

When no kernel is specified, `stock` is used as the default and auto-downloaded on first `vm create`.

## Networking

### Model: TAP + NAT per VM

Each VM gets a point-to-point link:

```
Host: em-<short-id> (TAP)  10.100.0.1/30  ←→  Guest: eth0  10.100.0.2/30
```

Traffic between two VMs is routed by the host across their TAPs, since
each sits on its own /30. What is and isn't permitted across those links
is the firewall policy below.

### IP Allocation

- Configurable base subnet (default: `10.100.0.0/16`)
- Sequential /30 blocks: `10.100.0.0/30`, `10.100.0.4/30`, `10.100.0.8/30`, ...
- Host gets .1, guest gets .2 in each /30
- Supports ~16384 concurrent VMs with a /16
- Allocations tracked in state store, released on VM delete

### Firewall Policy

One contract, independent of whatever else the host keeps in its
firewall: **a VM reaches the internet and the other VMs of its own
installation, and nothing else.** In particular it cannot reach the
host, at any host address.

Each installation owns two chains, entered from position 1 of the
built-in chains. Position 1 is what makes the policy hold: a
pre-existing `-A INPUT -s 10.0.0.0/8 -j ACCEPT` would otherwise match
guest traffic before the host block, and a mid-chain `REJECT` from ufw
or firewalld would be reached before the forwarding rules. Both chains
are transparent to non-ember traffic, since every rule in them matches
an ember TAP interface and anything else falls through to the built-in
chain right after the jump.

```
-A INPUT   -j ember-<id>-input
-A FORWARD -j ember-<id>-forward

# ember-<id>-input: host isolation.
-i em<id>-+ -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
-i em<id>-+ -j DROP

# ember-<id>-forward: install-wide.
-i em<id>-+ -o em<id>-+ -j ACCEPT      # VM to VM, both directions
-i em<id>-+ -j DROP                    # terminal

# ember-<id>-forward: per VM.
-i <tap-dev> -o <wan-iface> -j ACCEPT
-i <wan-iface> -o <tap-dev> -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
```

The established-accept in the input chain is mandatory. Replies to
host-initiated connections arrive on INPUT from the TAP, so without it
`ember ssh`, `exec` and `cp` would break.

Rule order needs no bookkeeping. Each chain holds ACCEPTs plus exactly
one terminal DROP, ACCEPTs are inserted at the front and the DROP is
appended, so the DROP is always last whatever subset of rules already
exists.

Chains are asserted on every VM start, not created once at `ember
init`, because iptables state does not survive a reboot. They are
removed by `ember deinit`. The chain a VM's rules went into is recorded
on its `NetworkInfo`, so rules written by an older binary (appended
straight to FORWARD with a `-m comment --comment ember:<id>` tag) are
still deleted from where they actually are.

Masquerade stays in the shared `nat` POSTROUTING chain with its comment
tag. It is an address translation rather than a policy decision, and
keeping its shape unchanged means rules written before and after the
policy chains existed remain mutually deletable.

### Setup (per VM start)

1. Create TAP device via ioctl (`/dev/net/tun`, IFF_TAP | IFF_NO_PI)
2. Disable IPv6 on the device (`net.ipv6.conf.<tap>.disable_ipv6=1`),
   before the link comes up so no link-local address is ever assigned.
   ember is IPv4-only, and a v6 link-local pair would let a guest reach
   the host past the IPv4 policy.
3. `ip addr add <host-ip>/30 dev em-<short-id>` + `ip link set up`
4. Enable IP forwarding: `sysctl net.ipv4.ip_forward=1`
5. Create the installation's chains and jumps if absent (idempotent)
6. Add the per-VM masquerade and forwarding rules

### Cleanup (per VM stop/delete)

1. `iptables -D` for the per-VM rules, in the chain recorded on the VM
2. `ip link delete em-<short-id>`
3. Release IP allocation

The installation's chains outlive its VMs and are removed by `ember
deinit`.

### WAN Interface Detection

Runs `ip route get 8.8.8.8` and parses the `dev <iface>` field. Auto-detected during `ember init` and cached in the global config (`config.json` `wan_iface` field). Can be overridden with `ember init --wan-iface <iface>`. At VM start time, falls back to re-detection if not cached.

### Guest DNS

Guest VMs need DNS servers reachable through their NATed network path. The host's nameservers are detected and passed via the kernel `ip=` boot parameter, which populates `/proc/net/pnp` in the guest. The rootfs injection step symlinks `/etc/resolv.conf` to `/proc/net/pnp`, so DNS configuration is dynamic per boot.

Detection order (scoped to the WAN interface to avoid unreachable servers):

1. `resolvectl dns <wan-iface>` — per-interface DNS from systemd-resolved
2. `/run/systemd/resolve/resolv.conf` — upstream servers from systemd-resolved (avoids 127.0.0.53 stub)
3. `/etc/resolv.conf` — direct resolv.conf parsing
4. Fallback: `1.1.1.1`, `8.8.8.8`

Filters out IPv6 addresses (VMs only have IPv4), loopback addresses, and the host's own addresses. All three are unreachable from a guest, the last because the firewall policy blocks VM-to-host traffic, so a host running its own resolver (dnsmasq, or a pihole bound to the LAN address) would otherwise hand every guest a nameserver that answers nothing. A dropped server falls through to the next detection source and is reported as a warning. Returns at most 2 servers (kernel `ip=` parameter limit).

### Rootfs Injection

During image pull and build, the following files are injected into the unpacked rootfs before the ext4 image is created:

- **SSH `authorized_keys`**: The invoking user's default public key is written to `/root/.ssh/authorized_keys` (or `/home/ubuntu/.ssh/authorized_keys` for built images). Permissions: `.ssh/` at 700, `authorized_keys` at 600. For non-root users, files are chowned to match the home directory owner (required by OpenSSH `StrictModes`).

- **`/etc/resolv.conf`**: Replaced with a symlink to `/proc/net/pnp`. The kernel populates this file from the `ip=` boot parameter's DNS fields, so DNS configuration is dynamic per boot without baking addresses into the image.

- **`/etc/inittab`** (pulled images only, skipped for built images): A minimal busybox-init-compatible inittab that maps Ctrl+Alt+Del to `/sbin/reboot` (required for Firecracker's `SendCtrlAltDel` graceful shutdown), spawns a login shell on `ttyS0`, and runs OpenRC init scripts if present. Built images use systemd which handles these natively.

## State Management

### State Directory (`/var/lib/ember/`)

```
/var/lib/ember/
├── config.json
├── kernels/
│   └── vmlinux-6.1.102         # stock preset
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
    pub boot_args: Option<String>,    // Custom boot args (replaces defaults, ip= still appended)
    pub subnet: Option<String>,       // Network subnet for IP allocation
    pub network: Option<NetworkInfo>,
    pub pid: Option<u32>,
    pub api_socket: PathBuf,
    pub created_at: String,
    pub ssh: SshConfig,
    pub forked_from: Option<String>,  // Origin snapshot path if forked
}
```

### Concurrency

- Per-VM files: independent, no contention
- Shared files (allocations.json, registry.json): `flock(LOCK_EX)` on write, `flock(LOCK_SH)` on read
- Atomic writes: write to temp file, then `rename()` to final path

### Crash Recovery

On every privileged command invocation (skipped for `init`, `version`, read-only queries, and SSH-client commands), lightweight reconciliation runs (`state/reconcile.rs`):
- For each VM in Running or Paused state, check if PID is alive (`kill(pid, 0)`)
- Dead process → mark Stopped, cleanup TAP + iptables + IP allocation
- A running VM whose forwarding rules are outside the installation's policy chain → move them in, so the chain's terminal DROP doesn't cut off a VM that was started before the chain existed
- Orphaned `em-*` TAP devices without running VM → delete

Reconciliation can also be triggered manually via `ember reconcile`.

All reconciliation operations are best-effort: errors are logged but never propagated, so reconciliation never blocks normal CLI operation.

### Rollback Guards

Multi-step operations (VM create, VM start, image pull, image build) use an RAII rollback guard (`cleanup.rs`) to clean up partial state on failure. Each successful resource creation registers a cleanup closure. If the operation fails (due to `?` early return), the guard's `Drop` implementation executes all registered cleanups in LIFO order. If the operation succeeds, `commit()` disarms the guard.

For example, during `vm start`:
1. IP allocation → registers release on rollback
2. TAP device creation → registers deletion on rollback
3. iptables rules → registers rule removal on rollback
4. Firecracker process → registers kill on rollback
5. All steps succeed → `commit()` keeps all resources

### Cleanup on VM Delete

1. Stop if running (or `--force` → SIGKILL)
2. Remove iptables rules
3. Delete TAP device
4. Release IP allocation
5. `zfs destroy` zvol (and any internal fork snapshots beneath it)
6. Remove state directory

Each step is idempotent — continues if resource already gone.

### Cleanup on Image Delete

`ember image delete <name>` removes the image from the local registry and destroys its ZFS zvol (including the `@base` snapshot).

If VMs were cloned from the image, they hold a ZFS dependency on the `@base` snapshot. Without `--force`, the command lists the dependent VMs and refuses to delete. With `--force`, it cascade-deletes all dependent VMs first (force-killing any that are running), then destroys the image zvol and removes the registry entry.

## Guest Access (SSH-based)

No custom guest agent initially. All guest interaction over SSH:

- **exec**: Open SSH channel, run command, stream stdout/stderr, return exit code
- **cp**: SCP-style file transfer (both directions, detected by `<vm-name>:` prefix)
- **ssh**: Convenience wrapper for interactive SSH session

SSH readiness: exponential backoff retry after VM boot, up to ~30s timeout.

Authentication: The invoking user's SSH public key (auto-discovered from `~/.ssh/`: prefers `id_ed25519.pub`, then `id_ecdsa.pub`, then `id_rsa.pub`) is injected at both image pull/build time and VM creation time. The SSH user is auto-detected (`ubuntu` if `/home/ubuntu` exists, otherwise `root`) and can be overridden in the YAML config.

Future: custom Rust agent over virtio-vsock for exec/cp without requiring SSH.
