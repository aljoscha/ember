# Ember — btrfs Storage Backend

This document specifies how ember supports btrfs as an alternative to ZFS for copy-on-write VM storage on Linux. Both backends coexist — the choice is made at `ember init` time and recorded in the global config.

## Design Principles

- **Same CLI, different storage**: All `ember` commands work identically regardless of whether ZFS or btrfs is the active backend. The storage difference is invisible to users after `ember init`.
- **Reflink clones**: `cp --reflink=always` provides instant copy-on-write clones of disk image files, analogous to `zfs clone` (Linux/ZFS) and `cp -c` (macOS/APFS).
- **File-based images**: VM root disks are raw ext4 `.img` files on a btrfs filesystem, passed directly to Firecracker via `path_on_host`. No zvols, no loopback devices.
- **Managed filesystem**: `ember init` creates and mounts the btrfs filesystem, just like it creates ZFS pools. Supports both block devices and file-backed images.
- **Transparent compression**: All mounts use `compress=zstd:3` for transparent compression. VM disk images compress well (~2-2.5x typical), significantly reducing storage usage. Comparable to ZFS's built-in compression.
- **Root required**: Same as ZFS — `mkfs.btrfs`, `mount`, loop mounting for SSH key injection, and Firecracker all need root.

## Component Mapping

| ZFS | btrfs | Notes |
|-----|-------|-------|
| `zpool create pool /dev/sda` | `mkfs.btrfs /dev/sda` + `mount` | btrfs has no pool concept; just a mounted filesystem |
| `zfs create pool/images` | `mkdir images/` | Directories replace ZFS datasets |
| `zfs create -V 10G pool/images/x` (zvol) | `cp image.img images/x.img` | Regular file replaces block device |
| `zfs snapshot pool/images/x@base` | Not needed | The `.img` file itself is the base; no snapshot layer |
| `zfs clone pool/images/x@base pool/vms/y` | `cp --reflink=always images/x.img vms/y/rootfs.img` | Instant CoW clone |
| `zfs snapshot pool/vms/y@snap` | `cp --reflink=always rootfs.img snapshots/snap.img` | Snapshot is a reflink copy |
| `zfs rollback pool/vms/y@snap` | `cp --reflink=always snap.img rootfs.img` | Replace rootfs with snapshot clone |
| `zfs destroy pool/vms/y@snap` | `rm snapshots/snap.img` | Just delete the file |
| `zfs set volsize=20G pool/vms/y` | `truncate -s 20G rootfs.img` | Grow the sparse file |
| `zfs destroy -r pool/vms/y` | `rm -rf vms/y/` | Delete directory tree |
| `/dev/zvol/pool/vms/y` | `/var/lib/ember/btrfs/vms/y/rootfs.img` | File path replaces block device path |

## Backend Selection

### `ember init`

The `--storage` flag selects the backend. It defaults to `zfs` for backward compatibility.

```bash
# ZFS (existing behavior, unchanged)
ember init --pool tank --device /dev/sda

# btrfs with block device
ember init --storage btrfs --device /dev/sdb

# btrfs with file-backed image
ember init --storage btrfs --device /path/to/btrfs.img --size 50G
```

When `--storage btrfs` is specified:
- `--device` is required (block device or file path)
- `--size` is required when `--device` is a file path (creates a sparse file of that size)
- `--pool` and `--dataset` are ignored
- The btrfs filesystem is mounted at `/var/lib/ember/btrfs` by default

### Runtime Dispatch

The storage backend type is recorded in `config.json`:

```json
{
  "storage_backend": "btrfs",
  "data_dir": "/var/lib/ember/btrfs",
  "kernel_path": "/var/lib/ember/kernels/vmlinux-6.1.102",
  "wan_iface": "eth0"
}
```

For ZFS configs (existing or new), the file looks the same as before. The `storage_backend` field defaults to `"zfs"` when absent, preserving backward compatibility with existing installations.

The `Storage` type becomes a runtime enum that delegates to `LinuxStorage` (ZFS) or `BtrfsStorage` based on the config.

## Storage Layout

```
/var/lib/ember/btrfs/               # btrfs mount point
├── images/
│   └── library-alpine-latest.img   # Base ext4 image (raw file)
└── vms/
    └── myvm/
        ├── rootfs.img              # Reflink clone of base image
        └── snapshots/
            ├── snap1.img           # Reflink clone of rootfs at snapshot time
            └── snap2.img
```

The state directory (`/var/lib/ember/`) remains on the root filesystem and holds `config.json`, `kernels/`, `images/registry.json`, `vms/<name>/vm.json`, and `network/allocations.json` — the same as with ZFS. Only the actual disk image files live on the btrfs filesystem.

## Initialization

### Block Device

```bash
ember init --storage btrfs --device /dev/sdb
```

1. Format: `mkfs.btrfs -f /dev/sdb`
2. Create mount point: `mkdir -p /var/lib/ember/btrfs`
3. Mount: `mount -o compress=zstd:3 /dev/sdb /var/lib/ember/btrfs`
4. Create directories: `mkdir -p /var/lib/ember/btrfs/{images,vms}`
5. Record device in config for remounting on next use

### File-Backed

```bash
ember init --storage btrfs --device /var/lib/ember/btrfs.img --size 50G
```

1. Create sparse file: `truncate -s 50G /var/lib/ember/btrfs.img`
2. Format: `mkfs.btrfs /var/lib/ember/btrfs.img`
3. Create mount point: `mkdir -p /var/lib/ember/btrfs`
4. Mount: `mount -o loop,compress=zstd:3 /var/lib/ember/btrfs.img /var/lib/ember/btrfs`
5. Create directories: `mkdir -p /var/lib/ember/btrfs/{images,vms}`
6. Record file path in config for remounting on next use

### Remounting

If the btrfs filesystem is not mounted when ember runs (e.g., after a reboot), ember auto-mounts it using the device or file path recorded in `config.json`, with the same `compress=zstd:3` mount option. This happens early in any command that accesses storage.

## Image Pull Workflow

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
ext4 image file (/tmp/ember-image-XXXX/image.ext4)
    │  (cp to btrfs)
    ▼
Base image: /var/lib/ember/btrfs/images/library-alpine-latest.img
```

The pipeline is the same as ZFS up to the ext4 image file. The final step copies (or moves) the ext4 file to the btrfs images directory instead of `dd`-ing to a zvol and creating a `@base` snapshot. The base image file itself serves the role of ZFS's `@base` snapshot — it's the immutable source for reflink clones.

## VM Create (Instant Reflink Clone)

```bash
cp --reflink=always /var/lib/ember/btrfs/images/library-alpine-latest.img \
                    /var/lib/ember/btrfs/vms/myvm/rootfs.img
```

This is instant regardless of image size (btrfs copy-on-write). The raw image file path is passed directly to Firecracker as `path_on_host` for the root drive.

After cloning, the rootfs is loop-mounted (`mount -o loop rootfs.img /tmp/...`) to inject per-VM SSH keys, then unmounted.

### `--reflink=always` Failure Detection

`cp --reflink=always` fails with an error rather than silently falling back to a full copy:
- Non-btrfs filesystem: `"failed to clone: Operation not supported"`
- Cross-device: `"failed to clone: Invalid cross-device link"`

Ember catches these and reports a clear message:

```
Error: VM storage requires a btrfs filesystem with reflink support.
The data directory /var/lib/ember/btrfs/ is not on a btrfs filesystem.
```

### Timing-Based Sanity Check

As with APFS, `ember vm create` measures the wall-clock time of the `cp --reflink=always` operation. A CoW clone completes in milliseconds. If it takes longer than 1 second for a multi-GB image, log a warning:

```
Warning: disk clone took 3.2s — this may indicate copy-on-write is not working.
```

## VM Resize

```bash
ember vm resize myvm --disk-size 8G
```

1. VM must be stopped
2. `truncate -s 8G /var/lib/ember/btrfs/vms/myvm/rootfs.img` — grows the sparse file
3. `e2fsck -f -p /var/lib/ember/btrfs/vms/myvm/rootfs.img` — check filesystem
4. `resize2fs /var/lib/ember/btrfs/vms/myvm/rootfs.img` — expand ext4

Both `e2fsck` and `resize2fs` operate directly on image files (no loop mount needed for resize). Shrinking is not supported.

## User Snapshots

```bash
# Create: reflink clone current state
ember snapshot create myvm snap1
→  cp --reflink=always vms/myvm/rootfs.img vms/myvm/snapshots/snap1.img

# Restore: replace rootfs with snapshot clone (VM must be stopped)
ember snapshot restore myvm snap1
→  cp --reflink=always vms/myvm/snapshots/snap1.img vms/myvm/rootfs.img.tmp
→  mv vms/myvm/rootfs.img.tmp vms/myvm/rootfs.img

# List: read snapshot directory
ember snapshot list myvm
→  ls vms/myvm/snapshots/*.img  (stat each for size and mtime)

# Delete: remove snapshot file
ember snapshot delete myvm snap1
→  rm vms/myvm/snapshots/snap1.img
```

### Atomic Restore

Snapshot restore uses a two-step process for atomicity: reflink clone to a `.tmp` file, then `mv` (rename) to the final path. `mv` within the same filesystem is atomic — if interrupted, either the old or new file is present, never a partial copy.

## VM Fork (Instant Clone)

```bash
ember vm fork source newvm
```

1. Create snapshot of source: `cp --reflink=always vms/source/rootfs.img vms/source/snapshots/fork-newvm.img`
2. Clone snapshot for target: `cp --reflink=always vms/source/snapshots/fork-newvm.img vms/newvm/rootfs.img`
3. If `--disk-size` is larger, grow with `truncate` + `resize2fs`
4. Loop-mount and inject SSH key
5. Start the forked VM (unless `--no-start`)

The `fork-newvm.img` snapshot on the source VM tracks the fork origin, stored in `vm.json` as `forked_from`.

**Cleanup:**
- Deleting a forked VM removes its directory and the fork snapshot file on the source.
- Unlike ZFS, reflink files are independent after creation — deleting the source snapshot doesn't affect the forked VM. But we keep the cleanup for consistency and to avoid accumulating stale fork snapshots.

## Firecracker Integration

The only Firecracker change is what path is passed as `path_on_host` for the root drive:

| Backend | `path_on_host` |
|---------|----------------|
| ZFS | `/dev/zvol/tank/ember/vms/myvm` (block device) |
| btrfs | `/var/lib/ember/btrfs/vms/myvm/rootfs.img` (regular file) |

Firecracker accepts both. All other Firecracker configuration (CPU, memory, kernel, network, boot args) is identical.

## VM Metadata

The `zvol_path` field in `VmMetadata` is renamed to `disk_path` to be backend-agnostic:

```rust
pub struct VmMetadata {
    // ...
    /// Storage path for the VM's root disk.
    /// ZFS: zvol name (e.g., "tank/ember/vms/myvm")
    /// btrfs: file path (e.g., "/var/lib/ember/btrfs/vms/myvm/rootfs.img")
    #[serde(alias = "zvol_path")]
    pub disk_path: String,
    /// Origin snapshot if forked.
    /// ZFS: "tank/ember/vms/source@fork-newvm"
    /// btrfs: "/var/lib/ember/btrfs/vms/source/snapshots/fork-newvm.img"
    pub forked_from: Option<String>,
    // ...
}
```

The `#[serde(alias = "zvol_path")]` ensures backward compatibility with existing `vm.json` files.

Similarly, `ImageEntry.zvol` becomes `ImageEntry.disk_path` with `#[serde(alias = "zvol")]`.

## Image Dependency Tracking

ZFS naturally prevents deleting an image zvol that has dependent clones (the `zfs destroy` fails). With btrfs reflinks, the base image file can be deleted even while VMs cloned from it exist — reflink blocks are reference-counted at the filesystem level, so VMs are unaffected.

However, the existing image registry already tracks which images exist, and `ember image delete` already checks for dependent VMs before deleting. This logic works unchanged for btrfs.

## Module Structure

```
src/
├── backend/
│   ├── mod.rs              # Trait defs + Storage enum (Zfs | Btrfs) + runtime dispatch
│   ├── linux/
│   │   ├── mod.rs
│   │   ├── vm.rs           # Firecracker (handles both zvol paths and file paths)
│   │   ├── storage.rs      # ZFS StorageBackend impl (unchanged)
│   │   ├── btrfs.rs        # btrfs StorageBackend impl (new)
│   │   ├── network.rs      # TAP + iptables (unchanged)
│   │   └── image.rs        # ext4 creation with loop mount (unchanged)
│   └── macos/              # (future)
```

## Comparison: ZFS vs btrfs vs APFS

| Operation | ZFS (Linux) | btrfs (Linux) | APFS (macOS) |
|-----------|-------------|---------------|--------------|
| Init | `zpool create` + `zfs create` | `mkfs.btrfs` + `mount` + `mkdir` | `mkdir` |
| Base image | zvol + `@base` snapshot | Raw `.img` file | Raw `.img` file |
| VM clone | `zfs clone x@base y` | `cp --reflink=always x.img y.img` | `cp -c x.img y.img` |
| Snapshot | `zfs snapshot y@snap` | `cp --reflink=always` | `cp -c` |
| Restore | `zfs rollback y@snap` | `cp --reflink=always` + `mv` | `cp -c` |
| Delete snap | `zfs destroy y@snap` | `rm snap.img` | `rm snap.img` |
| Resize | `zfs set volsize` + `resize2fs` | `truncate` + `resize2fs` | `truncate` + `resize2fs` |
| Fork | `zfs snapshot` + `zfs clone` | Two `cp --reflink=always` | `cp -c` |
| Drive path | `/dev/zvol/...` (block device) | `.../rootfs.img` (file) | `.../rootfs.img` (file) |
| Root required | Yes | Yes | No |
| Filesystem validation | `zpool list` | `stat -f` or `/proc/mounts` | `diskutil info` |

The btrfs backend is structurally almost identical to the macOS APFS backend — both use file-based CoW clones. The main differences are the clone command (`cp --reflink=always` vs `cp -c`), the mount mechanism (`mount -o loop` vs `hdiutil attach`), and the init process (managed btrfs filesystem vs APFS-is-always-there).

## External Dependencies

- **`btrfs-progs`**: Provides `mkfs.btrfs`. Usually pre-installed on modern Linux distributions. Required for `ember init --storage btrfs`.
- **`e2fsprogs`**: Provides `mkfs.ext4`, `e2fsck`, `resize2fs`. Already required by the ZFS backend.
- **GNU coreutils 8.0+**: Provides `cp --reflink=always`. Available on all modern Linux distributions.
