# Ember — btrfs Storage Backend

This document specifies how ember will support btrfs as an alternative to ZFS for copy-on-write VM storage on Linux. The btrfs backend is **not yet implemented** — this is a design spec. The macOS backend (APFS clones) already uses the same file-based CoW approach that btrfs will use, so the btrfs implementation follows the same pattern.

## Design Principles

- **Same CLI, different storage**: All `ember` commands work identically regardless of whether ZFS or btrfs is the active backend. The storage difference is invisible to users after `ember init`.
- **Reflink clones**: `cp --reflink=always` provides instant copy-on-write clones of disk image files, analogous to `zfs clone` (Linux/ZFS) and `cp -c` (macOS/APFS).
- **File-based images**: VM root disks are raw ext4 `.img` files on a btrfs filesystem, passed directly to Firecracker via `path_on_host`. No zvols, no loopback devices.
- **Managed filesystem**: `ember init` creates and mounts the btrfs filesystem, just like it creates ZFS pools. Supports both block devices and file-backed images.
- **Transparent compression**: All mounts use `compress=zstd:3` for transparent compression. VM disk images compress well (~2-2.5x typical), significantly reducing storage usage. Comparable to ZFS's built-in compression.
- **Root required**: Same as ZFS — `mkfs.btrfs`, `mount`, loop mounting for SSH key injection, and Firecracker all need root.
- **Reflinks, not subvolumes**: btrfs also offers subvolume snapshots (`btrfs subvolume snapshot`), but we use reflink clones instead. Subvolume snapshots have parent-child relationships that would reintroduce the same dependency-tracking complexity as ZFS (fork snapshots, `storage_dependents`, deletion ordering). Reflink clones are fully independent — deleting the source doesn't affect clones — which matches the macOS/APFS model and keeps the implementation simple. Subvolumes also add lifecycle management overhead (async deletion via `btrfs subvolume delete` + `sync`, nested mount points) with no benefit over reflinks for our use case.

## Component Mapping

| ZFS | btrfs | Notes |
|-----|-------|-------|
| `zpool create pool /dev/sda` | `mkfs.btrfs /dev/sda` + `mount` | btrfs has no pool concept; just a mounted filesystem |
| `zfs create pool/images` | `mkdir images/` | Directories replace ZFS datasets |
| `zfs create -V 10G pool/images/x` (zvol) | `cp image.img images/x.img` | Regular file replaces block device |
| `zfs snapshot pool/images/x@base` | Not needed | The `.img` file itself is the base; no snapshot layer |
| `zfs clone pool/images/x@base pool/vms/y` | `cp --reflink=always images/x.img vms/y/rootfs.img` | Instant CoW clone |
| `zfs clone pool/vms/a@fork-b pool/vms/b` | `cp --reflink=always vms/a/rootfs.img vms/b/rootfs.img` | Fork is an independent reflink clone |
| `zfs set volsize=20G pool/vms/y` | `truncate -s 20G rootfs.img` | Grow the sparse file |
| `zfs destroy -r pool/vms/y` | `rm -rf vms/y/` | Delete directory tree |
| `/dev/zvol/pool/vms/y` | `/var/lib/ember/btrfs/vms/y/rootfs.img` | File path replaces block device path |

## Backend Selection

### `ember init`

A new `--storage` flag selects the backend. It defaults to `zfs` for backward compatibility.

```bash
# ZFS (existing behavior, unchanged)
ember init --pool tank --device /dev/sda

# btrfs with block device
ember init --storage btrfs --storage-path /dev/sdb

# btrfs with file-backed image
ember init --storage btrfs --storage-path /path/to/btrfs.img --size 50G
```

When `--storage btrfs` is specified:
- `--storage-path` is required (block device or file path). `--device` is ZFS-only and ignored.
- `--size` disambiguates: when present, `--storage-path` is treated as a file path and a sparse file of that size is created. When absent, `--storage-path` must be an existing block device.
- `--pool` and `--dataset` are ignored
- The btrfs filesystem is mounted at `/var/lib/ember/btrfs` by default

If a `config.json` already exists, `ember init` checks the existing `storage_backend` field and refuses to re-initialize with a different backend. Switching backends requires `ember deinit` first. This prevents accidentally mixing ZFS and btrfs state in the same state directory.

### Backend Dispatch

Currently, the storage backend is selected at compile time via `#[cfg(target_os)]` type aliases — `Storage` resolves to `LinuxStorage` (ZFS) on Linux and `MacosStorage` (APFS) on macOS. There is no runtime dispatch.

To support btrfs alongside ZFS on Linux, the dispatch mechanism needs to change. Two options:

**Option A — Trait object dispatch** (preferred): Change the Linux `Storage` type alias to a shared trait object:

```rust
// src/backend/mod.rs
#[cfg(target_os = "linux")]
pub type Storage = Arc<dyn StorageBackend>;
```

Backend selection happens at construction time based on `GlobalConfig`:

```rust
// crates/ember-linux/src/lib.rs
pub fn create_storage(config: &GlobalConfig) -> Arc<dyn StorageBackend> {
    match config.storage_backend {
        StorageKind::Btrfs => Arc::new(BtrfsStorage::new(config)),
        StorageKind::Zfs => Arc::new(ZfsStorage::new(config)),
    }
}
```

`Arc` is used instead of `Box` because the CLI code clones the storage backend into `move` closures for rollback guards (e.g., `src/cli/vm.rs`, `src/cli/image.rs`). `Arc::clone` is a cheap refcount bump, and the storage backend is only read after construction, so shared ownership fits naturally.

This avoids boilerplate delegation (no match arms for each of the ~15 trait methods) and is simple to extend with additional backends later. Performance is not a concern — storage operations shell out to external commands, so the vtable indirection is negligible. The `StorageBackend` trait will need to be made object-safe (it already is, except for `init` which has a `where Self: Sized` bound — see the `init` discussion below).

**Option B — Compile-time feature flag**: Use a cargo feature (`--features btrfs`) to swap the Linux storage implementation at build time. Simpler but means a single binary can't support both.

### Config Changes

`GlobalConfig` (currently at `crates/ember-core/src/config/mod.rs`) gains two new fields. The proposed struct with additions marked:

```rust
/// Which storage backend is active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    #[default]
    Zfs,
    Btrfs,
}

pub struct GlobalConfig {
    /// NEW: Which storage backend is active.
    /// Defaults to Zfs when absent (backwards compatibility).
    #[serde(default)]
    pub storage_backend: StorageKind,
    pub pool: String,              // ZFS only (existing)
    pub dataset: String,           // ZFS only (existing)
    pub kernel_path: Option<PathBuf>,
    pub wan_iface: Option<String>,
    pub state_dir: PathBuf,
    /// NEW: Block device or image file path for btrfs storage. Used for remounting.
    #[serde(default)]
    pub storage_path: Option<PathBuf>,
}
```

Using an enum rather than a string ensures invalid values (typos like `"btrf"`) are rejected at deserialization time, and makes the `create_storage()` match exhaustive.

For ZFS configs (existing or new), `storage_backend` defaults to `Zfs` when absent, preserving backward compatibility. The `pool` and `dataset` fields are ignored when `storage_backend` is `Btrfs`.

The btrfs mount point (`/var/lib/ember/btrfs`) is not stored in config — it is derived as a constant. This avoids a config field that is never user-configurable. If custom mount points are needed in the future, a `data_dir` field can be added then.

`InitConfig` (at `crates/ember-core/src/backend.rs`) carries init arguments to `StorageBackend::init`. It currently only has ZFS-centric fields (`pool`, `dataset`, `device`). It needs to be extended for btrfs:

```rust
pub struct InitConfig {
    pub state_dir: PathBuf,
    pub pool: String,              // ZFS only
    pub dataset: String,           // ZFS only
    pub device: Option<String>,    // ZFS only: block device for pool creation
    /// NEW: Block device or file path for btrfs storage.
    /// When `btrfs_size` is set, this is a file path (sparse image will be created).
    /// When `btrfs_size` is absent, this must be an existing block device.
    pub storage_path: Option<PathBuf>,
    /// NEW: Size for file-backed btrfs image (e.g., "50G"). Only used with
    /// `storage_path` when the storage backend is btrfs.
    pub btrfs_size: Option<String>,
}
```

### Init Dispatch

`StorageBackend::init` is an associated function (`fn init(config: &InitConfig)` with `where Self: Sized`) — it can't be called through `Arc<dyn StorageBackend>` because the trait object doesn't exist yet at init time. The `ember init` command dispatches directly based on the `--storage` flag:

```rust
// In the ember init command handler
match storage_backend {
    StorageKind::Btrfs => BtrfsStorage::init(&init_config)?,
    StorageKind::Zfs => ZfsStorage::init(&init_config)?,
}
```

This is the one place where the concrete types are named explicitly. All subsequent commands go through `Arc<dyn StorageBackend>` via `create_storage()`.

### Deinit Trait Method

The `StorageBackend` trait gains a new instance method for tearing down backend infrastructure:

```rust
/// Tear down the storage backend's infrastructure.
///
/// The inverse of `init`: unmounts filesystems, removes mount points,
/// cleans up backend-specific resources. Does not destroy block devices.
///
/// This is an instance method (unlike `init`) because the backend is
/// already constructed when deinit is called.
fn deinit(&self) -> Result<()>;
```

The corresponding CLI command is `ember deinit`, which is new for both backends:

- **btrfs**: unmount the btrfs filesystem, remove the mount point, optionally delete the backing image file.
- **ZFS**: `zpool destroy` to remove the pool (existing behavior, currently not exposed as a CLI command).

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

This layout mirrors the macOS APFS storage layout (`state_dir/images/data/*.img`, `state_dir/vms/*/rootfs.img`), except the disk images live on a separately mounted btrfs filesystem rather than alongside the state files.

## Initialization

### Block Device

```bash
ember init --storage btrfs --storage-path /dev/sdb
```

1. Format: `mkfs.btrfs -f /dev/sdb`
2. Create mount point: `mkdir -p /var/lib/ember/btrfs`
3. Mount: `mount -o compress=zstd:3 /dev/sdb /var/lib/ember/btrfs`
4. Create directories: `mkdir -p /var/lib/ember/btrfs/{images,vms}`
5. Record storage path in config for remounting on next use

### File-Backed

```bash
ember init --storage btrfs --storage-path /var/lib/ember/btrfs.img --size 50G
```

1. Create sparse file: `truncate -s 50G /var/lib/ember/btrfs.img`
2. Format: `mkfs.btrfs /var/lib/ember/btrfs.img`
3. Create mount point: `mkdir -p /var/lib/ember/btrfs`
4. Mount: `mount -o loop,compress=zstd:3 /var/lib/ember/btrfs.img /var/lib/ember/btrfs`
5. Create directories: `mkdir -p /var/lib/ember/btrfs/{images,vms}`
6. Record file path in config for remounting on next use

### Remounting

If the btrfs filesystem is not mounted when ember runs (e.g., after a reboot), ember auto-mounts it using the `storage_path` recorded in `config.json`. The mount options depend on whether `storage_path` is a regular file or block device (determined via `stat`): file-backed uses `mount -o loop,compress=zstd:3`, block devices use `mount -o compress=zstd:3`. This happens early in any command that accesses storage.

Ember does **not** modify `/etc/fstab`. Auto-mounting on demand is sufficient — ember already needs root, so it can mount when needed. Avoiding fstab keeps the init/deinit lifecycle self-contained (no system config files to clean up) and matches the ZFS approach where ZFS pools are imported on demand rather than via fstab.

### Filesystem Validation

Before any storage operation, ember checks that the btrfs mount point is actually mounted by reading `/proc/mounts` for an entry matching the configured mount point. If not mounted, it attempts auto-remount (see above). This is the btrfs equivalent of `zpool list` for ZFS.

No deeper health checks (like `btrfs device stats`) are performed — if the filesystem is mounted and accessible, it's considered healthy. Filesystem-level health monitoring is left to the system administrator, same as with ZFS where ember doesn't run `zpool scrub`.

### Teardown (`ember deinit`)

`ember deinit` is a new CLI command (not yet implemented for either backend) that tears down the storage backend — the inverse of `ember init`. It dispatches through `Arc<dyn StorageBackend>` via the `deinit()` trait method.

For the btrfs backend:

1. Unmount: `umount /var/lib/ember/btrfs`
2. If file-backed: delete the image file (e.g., `rm /var/lib/ember/btrfs.img`)
3. Remove mount point: `rmdir /var/lib/ember/btrfs`
4. Remove config: delete `config.json` (a full `ember init` is required to re-initialize)

For block devices, step 2 is skipped — the device is left intact (the user may want to reuse or wipe it manually). This mirrors ZFS where `zpool destroy` removes the pool metadata but leaves the block device.

For the ZFS backend, `deinit()` wraps the existing `zpool destroy` logic that is currently not exposed as a CLI command.

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

The pipeline is the same as ZFS up to the ext4 image file. The final step copies the ext4 file to the btrfs images directory instead of `dd`-ing to a zvol and creating a `@base` snapshot. Since the temp directory and btrfs mount are always on different filesystems, this is always a full copy (no rename optimization). The base image file itself serves the role of ZFS's `@base` snapshot — it's the immutable source for reflink clones.

After copying, the base image is made read-only (`chmod 444`) to prevent accidental writes. Unlike ZFS's `@base` snapshot which is filesystem-enforced read-only, a regular file needs explicit protection. `ember image delete` removes the read-only flag before unlinking.

This matches how `MacosStorage::create_image_volume` works: it copies the ext4 image into the images directory and returns the file path.

The `ember image build` workflow is identical — it also produces an ext4 image file (via `docker build` + `docker export` instead of `skopeo`), so the final import step is the same `cp` + `chmod 444`.

## VM Create (Instant Reflink Clone)

```bash
cp --reflink=always /var/lib/ember/btrfs/images/library-alpine-latest.img \
                    /var/lib/ember/btrfs/vms/myvm/rootfs.img
```

This is instant regardless of image size (btrfs copy-on-write). The raw image file path is passed directly to Firecracker as `path_on_host` for the root drive.

After cloning, the rootfs is loop-mounted to inject per-VM SSH keys and hostname, then unmounted. (The macOS backend uses `debugfs -w` for this since macOS can't mount ext4 natively, but on Linux we can use standard `mount`.)

The existing ZFS `LinuxStorage::mount` mounts a block device directly (`mount /dev/zvol/... /tmp/...`). `BtrfsStorage::mount` must pass `-o loop` to mount the image file:

```bash
mount -o loop /var/lib/ember/btrfs/vms/myvm/rootfs.img /tmp/ember-mount-XXXX
```

The default `inject_ssh_key` and `inject_hostname` implementations on `StorageBackend` call `self.mount()` / `self.unmount()`, so the btrfs backend just needs to implement those two methods with the loop flag — no override of the injection methods is needed.

### `--reflink=always` Failure Detection

`cp --reflink=always` fails with an error rather than silently falling back to a full copy:
- Non-btrfs filesystem: `"failed to clone: Operation not supported"`
- Cross-device: `"failed to clone: Invalid cross-device link"`

Ember catches these and reports distinct messages for each case:

```
# Non-btrfs filesystem
Error: Reflink clone failed — the data directory /var/lib/ember/btrfs/ is not
on a btrfs filesystem. VM storage requires btrfs with reflink support.

# Cross-device
Error: Reflink clone failed — source and destination are on different filesystems.
Ensure the base image and VM storage are on the same btrfs mount.
```

### Timing-Based Sanity Check

Same as the macOS backend: `ember vm create` measures the wall-clock time of the `cp --reflink=always` operation. A CoW clone completes in milliseconds. If it takes longer than 1 second for a multi-GB image, log a warning:

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

Both `e2fsck` and `resize2fs` operate directly on image files (no loop mount needed for resize). Shrinking is not supported. The approach is the same as `MacosStorage::resize` (truncate + e2fsck + resize2fs), though `e2fsck` uses `-f -p` to match the existing Linux convention.

## VM Fork (Instant Clone)

```bash
ember vm fork source newvm
```

Like the macOS/APFS backend, btrfs forks are independent — a direct `cp --reflink=always` from the source VM's rootfs to the target VM's rootfs. No intermediate snapshot, no dependency chain between source and target.

1. Clone source disk for target: `cp --reflink=always vms/source/rootfs.img vms/newvm/rootfs.img`
2. If `--disk-size` is larger, grow with `truncate` + `resize2fs`
3. Loop-mount and inject SSH key + hostname
4. Start the forked VM (unless `--no-start`)

The `parent_vm` field in `VmMetadata` records the fork origin for informational purposes. Unlike ZFS where fork snapshots create a real dependency chain (deleting a source VM requires checking for dependent forks), btrfs forks are fully independent:

- `cleanup_fork` is a no-op (same as macOS/APFS)
- `storage_dependents` always returns an empty vec (same as macOS/APFS)

## Firecracker Integration

The only Firecracker change is what path is passed as `path_on_host` for the root drive:

| Backend | `path_on_host` |
|---------|----------------|
| ZFS | `/dev/zvol/tank/ember/vms/myvm` (block device) |
| btrfs | `/var/lib/ember/btrfs/vms/myvm/rootfs.img` (regular file) |

Firecracker accepts both. All other Firecracker configuration (CPU, memory, kernel, network, boot args) is identical.

Currently, `LinuxVm::start` (at `crates/ember-linux/src/vm.rs`) converts `vm.disk_path` to a block device path via `zfs::volume::device_path()`. With btrfs, `disk_path` already contains the full file path, so no conversion is needed. The dispatch is simple: if `disk_path` starts with `/`, use it directly (it's already a file path — btrfs or APFS); otherwise, treat it as a ZFS zvol name and convert via `zfs::volume::device_path()`:

```rust
// ZFS dataset names cannot start with '/' (they are always relative,
// e.g., "tank/ember/vms/myvm"), while btrfs file paths are always
// absolute (e.g., "/var/lib/ember/btrfs/vms/myvm/rootfs.img").
// This invariant is enforced by ZFS itself — pool names must start
// with a letter — so the check is safe.
let rootfs_path = if vm.disk_path.starts_with('/') {
    PathBuf::from(&vm.disk_path)
} else {
    zfs::volume::device_path(&vm.disk_path)
};
```

## VM Metadata

The `disk_path` field in `VmMetadata` (at `crates/ember-core/src/state/vm.rs`) is already backend-agnostic:

```rust
pub struct VmMetadata {
    // ...
    /// Path to the root disk. On Linux, a ZFS zvol (e.g., "tank/ember/vms/myvm")
    /// or a btrfs file path (e.g., "/var/lib/ember/btrfs/vms/myvm/rootfs.img").
    /// On macOS, a raw disk image path (e.g., ".../vms/myvm/rootfs.img").
    #[serde(alias = "zvol_path")]
    pub disk_path: String,
    /// Name of the source VM if this VM was forked.
    #[serde(default, alias = "forked_from")]
    pub parent_vm: Option<String>,
    // ...
}
```

The `#[serde(alias = "zvol_path")]` ensures backward compatibility with existing `vm.json` files.

Similarly, `ImageEntry` (at `crates/ember-core/src/image/registry.rs`) already has the right shape:

```rust
pub struct ImageEntry {
    pub reference: String,
    pub local_name: String,
    #[serde(alias = "zvol")]
    pub disk_path: String,
    pub size_mib: u64,
    pub pulled_at: String,
}
```

No changes to these structs are needed for btrfs support.

## Image Dependency Tracking

ZFS naturally prevents deleting an image zvol that has dependent clones (the `zfs destroy` fails). With btrfs reflinks, the base image file can be deleted even while VMs cloned from it exist — reflink blocks are reference-counted at the filesystem level, so VMs are unaffected.

However, the existing image registry already tracks which images exist, and `ember image delete` already checks for dependent VMs before deleting. This logic works unchanged for btrfs.

## Crate Structure

The existing codebase uses workspace crates, not a flat module layout:

```
crates/
├── ember-core/                  # Shared traits and types
│   └── src/
│       ├── backend.rs           # StorageBackend, VmBackend, NetworkBackend traits
│       ├── config/mod.rs        # GlobalConfig
│       ├── state/vm.rs          # VmMetadata
│       └── image/registry.rs    # ImageEntry
│
├── ember-linux/                 # Linux backend (Firecracker + ZFS)
│   └── src/
│       ├── lib.rs               # LinuxStorage, LinuxVm, LinuxNetwork, LinuxPlatform
│       ├── storage.rs           # LinuxStorage impl StorageBackend (ZFS)
│       ├── vm.rs                # LinuxVm impl VmBackend (Firecracker)
│       ├── zfs/                 # ZFS CLI wrappers (pool, dataset, volume, snapshot)
│       └── zvol.rs              # Image-to-zvol pipeline
│
└── ember-macos/                 # macOS backend (AVF + APFS)
    └── src/
        ├── lib.rs
        ├── storage.rs           # MacosStorage impl StorageBackend (APFS clones)
        └── vm.rs                # MacosVm impl VmBackend (ember-vz)

src/
└── backend/mod.rs               # Type aliases: Storage, Vm, Network, CurrentPlatform
```

The btrfs backend would live in `ember-linux` alongside the existing ZFS code. The current `storage.rs` (which contains `LinuxStorage` as a ZFS-only struct) becomes the ZFS-only implementation (renamed to `zfs_storage.rs`), and a new `btrfs_storage.rs` provides the btrfs implementation. A factory function in `storage.rs` returns `Arc<dyn StorageBackend>` based on the config:

```
crates/ember-linux/src/
├── storage.rs         # create_storage() factory → Arc<dyn StorageBackend> (replaces current)
├── zfs_storage.rs     # ZfsStorage impl StorageBackend (renamed from current storage.rs)
├── btrfs_storage.rs   # BtrfsStorage impl StorageBackend (new)
├── zfs/               # ZFS CLI wrappers (unchanged)
├── btrfs/             # btrfs CLI wrappers (new): mkfs, mount, reflink clone
├── vm.rs              # LinuxVm — needs to handle both zvol paths and file paths
└── ...
```

`BtrfsStorage` mirrors `MacosStorage` — it holds a path and derives all image/VM/snapshot paths from it:

```rust
/// btrfs storage backend using reflink clones.
pub struct BtrfsStorage {
    /// btrfs mount point (e.g., "/var/lib/ember/btrfs").
    /// All image and VM disk paths are derived from this.
    data_dir: PathBuf,
    /// Device or image file path for auto-remounting.
    storage_path: PathBuf,
}
```

The type alias in `src/backend/mod.rs` changes from `ember_linux::LinuxStorage` to `Arc<dyn StorageBackend>`, constructed via `ember_linux::create_storage(config)`.

### Display and Platform Adaptations

`LinuxPlatform` (at `crates/ember-linux/src/platform.rs`) currently hardcodes ZFS-specific labels in its `Platform` trait methods. These need to branch on the active `StorageKind`:

- **`inspect_vm_extra`**: Show "Disk image" instead of "ZFS zvol" for the disk path when btrfs is active.
- **`inspect_image_extra`**: Same — "Disk image" instead of "ZFS zvol".
- **`info_extra`**: Show "Storage" / "btrfs", "Mount point" / `/var/lib/ember/btrfs`, and "Storage path" / the backing device or image file, instead of "ZFS pool" / "Dataset".
- **`init_hint`**: Return a btrfs-appropriate hint (e.g., `"Run: ember init --storage btrfs --storage-path <device>"`) when btrfs is configured, instead of the current ZFS-only hint. Since `init_hint` is called before config exists (it's shown when no config is found), it should show both options or a generic hint.

These methods will need access to the `GlobalConfig` (or at least `StorageKind`) to branch. Currently `info_extra` already takes `&GlobalConfig`, but `init_hint` and the inspect methods do not — their signatures may need adjustment.

## Comparison: ZFS vs btrfs vs APFS

| Operation | ZFS (Linux) | btrfs (Linux) | APFS (macOS) |
|-----------|-------------|---------------|--------------|
| Init | `zpool create` + `zfs create` | `mkfs.btrfs` + `mount` + `mkdir` | `mkdir` |
| Base image | zvol + `@base` snapshot | Raw `.img` file | Raw `.img` file |
| VM clone | `zfs clone x@base y` | `cp --reflink=always x.img y.img` | `cp -c x.img y.img` |
| Snapshot | `zfs snapshot y@snap` | `cp --reflink=always` | `cp -c` |
| Restore | `zfs rollback y@snap` | `cp --reflink=always` + `mv` | `cp -c` + `mv` |
| Delete snap | `zfs destroy y@snap` | `rm snap.img` | `rm snap.img` |
| Resize | `zfs set volsize` + `resize2fs` | `truncate` + `resize2fs` | `truncate` + `resize2fs` |
| Fork | `zfs snapshot` + `zfs clone` (creates dependency) | `cp --reflink=always` (independent) | `cp -c` (independent) |
| Drive path | `/dev/zvol/...` (block device) | `.../rootfs.img` (file) | `.../rootfs.img` (file) |
| Root required | Yes | Yes | No |
| Filesystem validation | `zpool list` | `/proc/mounts` (every operation) | APFS volume check at init |
| SSH key injection | `mount` (block device) | `mount -o loop` (image file) | `debugfs -w` |
| Fork cleanup | Removes fork snapshot on source | No-op | No-op |
| Storage dependents | Parses fork snapshots | Always empty | Always empty |

The btrfs backend is structurally almost identical to the macOS APFS backend — both use file-based CoW clones with independent forks. The main differences are the clone command (`cp --reflink=always` vs `cp -c`), the mount mechanism (`mount -o loop` vs `debugfs`), and the init process (managed btrfs filesystem vs APFS-is-always-there).

## Storage Efficiency Diagnostics

`ember storage usage` works for btrfs the same way it does for macOS/APFS, but `st_blocks` is not the way to measure it. On APFS a reflinked clone reports its origin's full `st_blocks` while costing nothing, because the field counts the blocks a file maps rather than the ones it owns. Assume btrfs behaves the same until measured, and measure it before writing the backend rather than after.

btrfs has the better tool for this anyway: `btrfs filesystem du` reports shared, exclusive and total per file directly, which maps onto `VolumeUsage` without a sweep of our own. That makes it the natural source for `exclusive` and `referenced`, where APFS has to reconstruct the same answer from physical extents.

## External Dependencies

- **`btrfs-progs`**: Provides `mkfs.btrfs`. Usually pre-installed on modern Linux distributions. Required for `ember init --storage btrfs`.
- **`e2fsprogs`**: Provides `mkfs.ext4`, `e2fsck`, `resize2fs`. Already required by the ZFS backend.
- **GNU coreutils 8.0+**: Provides `cp --reflink=always`. Available on all modern Linux distributions.
