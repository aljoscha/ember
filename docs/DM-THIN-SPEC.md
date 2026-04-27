# Ember — dm-thin Storage Backend

This document specifies how ember will support Linux device-mapper thin provisioning (`dm-thin`) as an alternative to ZFS for copy-on-write VM storage on Linux.
The dm-thin backend is **not yet implemented** — this is a design spec.
It mirrors the structure of `BTRFS-SPEC.md` and reuses the trait-object dispatch model introduced there.

The goal is the same as the btrfs spec: drop the ZFS kernel module dependency and the requirement for a dedicated pool device, while preserving block-level copy-on-write semantics that are already a tight fit with Firecracker (raw block drive, instant clones, real snapshots).

## Design principles

* **Same CLI, different storage**: All `ember` commands work identically regardless of which backend is active. Backend choice is invisible to users after `ember init`.
* **Block-level CoW**: dm-thin provides instant copy-on-write thin volumes and snapshots at the block layer, analogous to ZFS zvols + clones. No filesystem-level reflinks.
* **Block device drives**: VM root disks are exposed as `/dev/mapper/<name>` block devices and passed directly to Firecracker as `path_on_host`. Same drive shape as the existing ZFS path.
* **Sparse-file backing by default**: `ember init` creates two sparse files (metadata + data) on the existing filesystem and assembles them into a thin pool via `losetup` + `dmsetup`. A raw block device may be used instead, but is not required.
* **Kernel-builtin**: dm-thin is in-tree (`CONFIG_DM_THIN_PROVISIONING`), shipped by every mainstream distribution since ~2012. No DKMS, no out-of-tree module, no licensing friction with the kernel.
* **No filesystem on the pool**: The pool itself is a block-device factory. Each thin volume is independently formatted with ext4 (the same ext4 image pipeline used today). The pool does not see file-level structure.
* **Thin volumes and snapshots are the same primitive**: In dm-thin, a snapshot is just another thin volume that shares blocks with its source. Image base, VM disk, user snapshot, and fork all use the same `create_snap` call.
* **Random 64-bit thin ids**: Unlike ZFS where datasets are addressed by name, dm-thin volumes are addressed by numeric ids. Ember picks a random `u64` per volume and retries on the rare collision. The id is stored on the existing `VmMetadata`/`ImageEntry` records; no separate allocator state.
* **Root required**: Same as ZFS — `dmsetup`, `losetup`, `mount`, and Firecracker all need root.

## Component mapping

| ZFS | dm-thin | Notes |
|-----|---------|-------|
| `zpool create pool /dev/sda` | `truncate` + `losetup` + `dmsetup create ember-pool ... thin-pool ...` | Thin pool replaces ZFS pool |
| `zfs create pool/images` | (none) | No dataset hierarchy; the pool is flat |
| `zfs create -V 10G pool/images/x` (zvol) | `dmsetup message ember-pool 0 "create_thin <random_u64>"` + `dmsetup create ember-img-x` | Thin volume replaces zvol |
| `zfs snapshot pool/images/x@base` | `dmsetup message ember-pool 0 "create_snap <base_id> <src_id>"` | Snapshot is just another thin id |
| `zfs clone pool/images/x@base pool/vms/y` | `create_snap <vm_id> <base_id>` + `dmsetup create ember-vm-y` | Same `create_snap`; activate as device |
| `zfs snapshot pool/vms/y@snap` | suspend vm + `create_snap <snap_id> <vm_id>` + resume | Suspend ensures consistent on-disk state |
| `zfs rollback pool/vms/y@snap` | remove vm device + delete vm thin + `create_snap <new_vm_id> <snap_id>` + recreate vm device | Restore replaces the live volume |
| `zfs destroy pool/vms/y@snap` | `dmsetup message ember-pool 0 "delete <id>"` | Releases blocks back to the pool |
| `zfs set volsize=20G pool/vms/y` | `dmsetup suspend` + `dmsetup load` (new size) + `dmsetup resume` + `resize2fs` | Resize is a table reload |
| `zfs destroy -r pool/vms/y` | `dmsetup remove ember-vm-y` + `delete <id>` | Two-step: deactivate then free |
| `/dev/zvol/pool/vms/y` | `/dev/mapper/ember-vm-y` | Different path, same shape |

## Backend selection

### `ember init`

The `--storage` flag introduced in `BTRFS-SPEC.md` gains a third value: `dm-thin`.

```bash
# ZFS (existing)
ember init --pool tank --device /dev/sda

# btrfs (per BTRFS-SPEC.md)
ember init --storage btrfs --storage-path /var/lib/ember/btrfs.img --size 50G

# dm-thin with sparse files (default)
ember init --storage dm-thin --size 50G

# dm-thin with explicit data file location
ember init --storage dm-thin --storage-path /var/lib/ember/dm-thin --size 50G

# dm-thin on a raw block device
ember init --storage dm-thin --storage-path /dev/sdb
```

When `--storage dm-thin` is specified:

* `--storage-path` selects the directory holding `metadata.img` and `data.img` (file-backed mode), or the raw block device to use (device mode). Defaults to `/var/lib/ember/dm-thin` for file-backed mode.
* `--size` is required for file-backed mode and disambiguates from device mode. When present, two sparse files are created. When absent, `--storage-path` must be an existing block device.
* `--metadata-size` is optional and defaults to a value computed from `thin_metadata_size` (see "Pool sizing" below).
* `--block-size` is optional and defaults to `64K`. **Permanent** — cannot be changed after pool creation.
* `--pool`, `--dataset`, and `--device` are ZFS-only and ignored.

If a `config.json` already exists, `ember init` checks `storage_backend` and refuses to re-initialize with a different backend.
Switching backends requires `ember deinit` first.

### Backend dispatch

Same as in `BTRFS-SPEC.md`: `Storage` becomes `Arc<dyn StorageBackend>` on Linux, dispatched at construction time by a `create_storage()` factory:

```rust
// crates/ember-linux/src/lib.rs
pub fn create_storage(config: &GlobalConfig) -> Arc<dyn StorageBackend> {
    match config.storage_backend {
        StorageKind::Zfs => Arc::new(ZfsStorage::new(config)),
        StorageKind::Btrfs => Arc::new(BtrfsStorage::new(config)),
        StorageKind::DmThin => Arc::new(DmThinStorage::new(config)),
    }
}
```

`StorageKind` gains a `DmThin` variant.
The `--storage dm-thin` CLI flag accepts `dm-thin` and serializes as `dm-thin` (lowercase, hyphen) to match common usage.

### Init dispatch

`StorageBackend::init` remains an associated function. The `ember init` handler matches on the requested backend:

```rust
match storage_backend {
    StorageKind::Zfs => ZfsStorage::init(&init_config)?,
    StorageKind::Btrfs => BtrfsStorage::init(&init_config)?,
    StorageKind::DmThin => DmThinStorage::init(&init_config)?,
}
```

### Config changes

`GlobalConfig` extensions, building on the btrfs spec:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageKind {
    #[default]
    Zfs,
    Btrfs,
    DmThin,
}

pub struct GlobalConfig {
    #[serde(default)]
    pub storage_backend: StorageKind,
    pub pool: String,                  // ZFS only
    pub dataset: String,               // ZFS only
    pub kernel_path: Option<PathBuf>,
    pub wan_iface: Option<String>,
    pub state_dir: PathBuf,
    /// Block device or image file path. Used by btrfs and dm-thin.
    /// For dm-thin: directory containing metadata.img/data.img, or a raw device.
    #[serde(default)]
    pub storage_path: Option<PathBuf>,
    /// dm-thin pool block size in 512-byte sectors (default: 128 = 64KiB).
    /// Permanent at pool creation.
    #[serde(default)]
    pub dm_thin_block_size: Option<u32>,
}
```

The pool name (`ember-pool`) and device-mapper prefixes (`ember-img-`, `ember-vm-`) are constants — not user-configurable.
This keeps the config small and prevents collisions between concurrent ember installations on the same host. Multi-instance support is out of scope for this spec.

`InitConfig` extensions:

```rust
pub struct InitConfig {
    pub state_dir: PathBuf,
    pub pool: String,                  // ZFS only
    pub dataset: String,               // ZFS only
    pub device: Option<String>,        // ZFS only
    pub storage_path: Option<PathBuf>, // btrfs + dm-thin
    pub btrfs_size: Option<String>,    // btrfs only
    /// Size of the dm-thin data device (e.g., "50G").
    /// Required for file-backed mode, ignored for device mode.
    pub dm_thin_size: Option<String>,
    /// Override metadata device size. Defaults to `thin_metadata_size` output.
    pub dm_thin_metadata_size: Option<String>,
    /// Pool block size in sectors. Defaults to 128 (64KiB).
    pub dm_thin_block_size: Option<u32>,
}
```

### Deinit trait method

The `deinit()` method introduced in the btrfs spec applies here too. For dm-thin:

1. Deactivate every active thin volume: `dmsetup remove ember-vm-*`, `ember-img-*`.
2. Remove the pool: `dmsetup remove ember-pool`.
3. Detach loop devices: `losetup -d /dev/loopN /dev/loopM`.
4. If file-backed: optionally delete `metadata.img` and `data.img` (gated behind `--purge`, default keep).
5. Remove the directory if empty.
6. Delete `config.json`.

Block devices are left intact, same as ZFS `zpool destroy`.

## Thin id allocation

dm-thin addresses each volume by a numeric `dev_id` and the kernel
enforces `dev_id <= (1 << 24) - 1` in `drivers/md/dm-thin.c`:

```c
#define MAX_DEV_ID ((1ULL << 24) - 1)

if (*dev_id > MAX_DEV_ID) {
    DMWARN("Message received with invalid device id: %llu", *dev_id);
    return -EINVAL;
}
```

So the usable space is 24 bits.
Ember picks a random non-zero id within that range:

```rust
const MAX_DEV_ID: u64 = (1 << 24) - 1;

fn fresh_thin_id() -> u64 {
    loop {
        let id = (rand::random::<u32>() as u64) & MAX_DEV_ID;
        if id != 0 {
            return id;
        }
    }
}

fn allocate(pool: &str) -> Result<u64> {
    loop {
        let id = fresh_thin_id();
        match dmsetup_message(pool, &format!("create_thin {id}")) {
            Ok(()) => return Ok(id),
            Err(e) if is_already_exists(&e) => continue,
            Err(e) => return Err(e),
        }
    }
}
```

Why this is safe:

* Birthday collision in a 24-bit space first crosses 1% probability around 1800 active ids. Realistic ember pools hold dozens to a few hundred volumes — well below that, and the kernel still rejects duplicates atomically (`EEXIST`) so the retry loop is the entire concurrency story.
* Two ember processes racing on `create_thin` cannot both succeed for the same id; whoever lost retries.
* No persistent counter, no allocator file, no flock around id generation.

`create_snap` follows the same pattern (allocate id, retry on `EEXIST`).
The `id` is recorded on the relevant `VmMetadata`/`ImageEntry`/`SnapshotEntry` under whichever lock already protects that record; the kernel pool itself remains the source of truth for liveness, queryable via `thin_dump` for recovery.

The serialized type on those records stays `u64` so the on-disk format does not need to change if the kernel ever lifts the 24-bit cap.
For now only the low 24 bits are populated.

## Pool sizing

The metadata device must be sized to cover the maximum number of blocks the pool can ever reference:

* Recommended formula: `metadata_size = max(48 * data_size / block_size, 2 MiB)` (kernel docs).
* Practical cap: 16 GiB. The kernel rejects metadata devices larger than this.
* Standard tool: `thin_metadata_size --block-size=64k --pool-size=50G --max-thins=1000 --numeric-only --unit=b`.

Defaults used by `ember init`:

* `block_size`: 64 KiB (128 sectors). Smaller block sizes give better sharing across snapshots at the cost of more metadata; 64 KiB is the documented kernel default.
* `metadata_size`: computed via `thin_metadata_size` for the requested data size, capped at 16 GiB, floor of 32 MiB.
* `low_water_mark`: `data_size / block_size / 16` blocks (≈6.25% of pool). When free blocks fall below this, the kernel notifies userspace via `dmeventd`. Ember does not register a userspace handler in this initial spec — the value is informational. A future enhancement could surface low-space warnings via `dmsetup status`.

## Storage layout

```
/var/lib/ember/dm-thin/             # Default --storage-path
├── metadata.img                    # Sparse file, ~32 MiB to 16 GiB
└── data.img                        # Sparse file, sized to --size

/var/lib/ember/                     # State directory (unchanged location)
├── config.json
├── kernels/
├── images/
│   └── registry.json               # ImageEntry records, now include thin_id
├── vms/
│   └── <name>/
│       └── vm.json                 # VmMetadata, includes thin_id
└── network/
```

No separate allocator state file is needed.
Thin ids live exclusively on `ImageEntry.thin_id`, `VmMetadata.thin_id`, and `SnapshotEntry.thin_id`.
Fresh ids are picked at random; the pool itself is the authority for which ids are live (queryable via `thin_dump /dev/loopMETA` for recovery).

## Initialization

### File-backed (default)

```bash
ember init --storage dm-thin --size 50G
```

1. Create directory: `mkdir -p /var/lib/ember/dm-thin`.
2. Compute metadata size: `thin_metadata_size --block-size=64k --pool-size=50G --max-thins=1000 --numeric-only --unit=b` → e.g. `838860800` (≈800 MiB).
3. Create sparse data: `truncate -s 50G /var/lib/ember/dm-thin/data.img`.
4. Create sparse metadata: `truncate -s 800M /var/lib/ember/dm-thin/metadata.img`.
5. Zero metadata header: `dd if=/dev/zero of=/var/lib/ember/dm-thin/metadata.img bs=4K count=1 conv=notrunc`. The kernel uses the all-zero superblock as the signal to format a fresh pool.
6. Attach loops: `losetup -f --show /var/lib/ember/dm-thin/metadata.img` → `/dev/loopN`; same for `data.img` → `/dev/loopM`.
7. Assemble pool: `dmsetup create ember-pool --table "0 <data_sectors> thin-pool /dev/loopN /dev/loopM 128 32768"` where `data_sectors = data_size / 512` and `32768` is the low-water mark in blocks.
8. Write `config.json` with `storage_backend = "dm-thin"`, `storage_path = /var/lib/ember/dm-thin`.

### Device-backed

```bash
ember init --storage dm-thin --storage-path /dev/sdb
```

1. Allocate metadata partition: requires either a separate metadata device or a partition layout. To avoid forcing partitioning on the user, ember uses **embedded metadata mode**: it places `metadata.img` as a sparse file on the state directory's filesystem and uses `--storage-path` only as the data device. (Splitting metadata onto a tiny separate device is a future enhancement.)
2. Wipe the device's first 4 KiB so the pool initializes fresh: `dd if=/dev/zero of=/dev/sdb bs=4K count=1`.
3. `losetup` only the metadata file. The data device is used directly.
4. Assemble pool: `dmsetup create ember-pool --table "0 <device_sectors> thin-pool /dev/loopN /dev/sdb 128 32768"`.

The init flow is otherwise identical.

### Activation on subsequent runs

dm-thin tables live only in kernel memory.
After a reboot or `dmsetup remove`, the pool and all thin volumes are gone from `/dev/mapper/` even though the underlying metadata is intact.
Ember therefore reactivates on demand.

The first command after a reboot triggers `ensure_pool_active`:

1. Read `config.json` → `storage_path`.
2. Check `/dev/mapper/ember-pool` exists. If yes, done.
3. If no:
   a. `losetup -f --show metadata.img` → `/dev/loopN` (skip if device-backed).
   b. `losetup -f --show data.img` → `/dev/loopM` (skip if device-backed).
   c. Run `thin_check /dev/loopN` (or the metadata loop). Fail loudly on metadata corruption — operator must run `thin_repair` manually.
   d. `dmsetup create ember-pool --table "0 <data_sectors> thin-pool ... 128 <low_water>"` using the values from `config.json`.

Per-VM and per-image volumes are activated **lazily** by methods that need them (e.g. `disk_device_path`, `mount`, `start`).
Each method calls `ensure_thin_active(name, thin_id, size_sectors)`:

1. If `/dev/mapper/<name>` exists, done.
2. Else: `dmsetup create <name> --table "0 <size_sectors> thin /dev/mapper/ember-pool <thin_id>"`.

Sizes come from existing `ImageEntry.size_mib` and `VmMetadata.disk_size_gib`.

### Filesystem validation

Before any storage operation, ember verifies `/dev/mapper/ember-pool` exists.
If not, it attempts the activation sequence above.
This is the dm-thin equivalent of the btrfs `/proc/mounts` check.

`dmsetup status ember-pool` is parsed to detect:

* `out_of_data_space`: pool is full. New writes will fail with EIO. Ember refuses VM create/start and prints an actionable error suggesting `ember storage grow`.
* `metadata_low_watermark`: metadata pressure. Logged as a warning.
* `read_only`: kernel switched the pool to read-only after a metadata error. Refuse all write operations.

### Teardown (`ember deinit`)

1. Stop all running VMs (precondition; ember refuses if any VM is running).
2. Remove all activated thin volumes: enumerate `dmsetup ls --target thin` filtered by the `ember-img-` / `ember-vm-` prefix, then `dmsetup remove` each.
3. Free thin ids: not strictly required (the next step destroys metadata) but done for symmetry: `dmsetup message ember-pool 0 "delete <id>"` for each.
4. Remove pool: `dmsetup remove ember-pool`.
5. Detach loops: `losetup -d /dev/loopN /dev/loopM`.
6. If `--purge`: delete `metadata.img`, `data.img`.
7. Remove `config.json`.

For device-backed pools, the data device is left intact — same as ZFS.

## Image pull workflow

Reuses the existing pipeline up to the ext4 image:

```
OCI registry → unpacked rootfs → mkfs.ext4 + populate → ext4 image file
                                                              │
                                                              ▼
                                              create_thin → activate → dd → snapshot
```

Per-image steps:

1. Allocate thin id: `id_a = fresh_thin_id()` (random `u64`, retry on collision — see "Thin id allocation" below).
2. Create thin: `dmsetup message ember-pool 0 "create_thin <id_a>"`.
3. Activate as a temporary device: `dmsetup create ember-img-<name>-staging --table "0 <size_sectors> thin /dev/mapper/ember-pool <id_a>"`.
4. Write image: `dd if=/tmp/ember-image-XXXX/image.ext4 of=/dev/mapper/ember-img-<name>-staging bs=1M`. Existing `zvol::dd_image` logic is reused once the device path is supplied.
5. Suspend: `dmsetup suspend ember-img-<name>-staging`. This forces a metadata commit so the snapshot below sees a consistent state.
6. Allocate base id: `id_base = fresh_thin_id()`.
7. Snapshot: `dmsetup message ember-pool 0 "create_snap <id_base> <id_a>"`.
8. Resume: `dmsetup resume ember-img-<name>-staging`.
9. Discard the staging device: `dmsetup remove ember-img-<name>-staging`. Free `id_a`: `dmsetup message ember-pool 0 "delete <id_a>"`. The `id_base` snapshot retains all of its blocks.
10. Persist: `ImageEntry.thin_id = id_base`, `disk_path = "/dev/mapper/ember-img-<name>"` (the activated path; lazy activation will create it on first use).

Why two ids? `create_snap` requires a source thin volume.
We need a snapshot of the freshly-written image so that VM clones can branch from a stable origin without our staging device hanging around as a dependency.
The pattern matches how ZFS uses `@base`: write to a primary, snapshot it, then never touch the primary again.

The base thin is not activated as a device by default; only VMs cloned from it appear in `/dev/mapper/`.
This keeps `/dev/mapper/` clutter-free and avoids races where a stale activation locks a volume.

## VM create

```bash
ember vm create myvm --image alpine --disk-size 4G
```

1. Look up `ImageEntry.thin_id` for `alpine` (the base id).
2. Allocate fresh id: `id_vm = fresh_thin_id()`.
3. Snapshot: `dmsetup message ember-pool 0 "create_snap <id_vm> <id_base>"`. Instant — no data is copied.
4. Activate: `dmsetup create ember-vm-myvm --table "0 <disk_sectors> thin /dev/mapper/ember-pool <id_vm>"`.
5. The activated device path `/dev/mapper/ember-vm-myvm` is recorded in `VmMetadata.disk_path` and `VmMetadata.thin_id = id_vm`.
6. Loop-mount via `mount /dev/mapper/ember-vm-myvm /tmp/...` to inject SSH key and hostname (the existing flow on the ZFS path; no `-o loop` needed because dm-thin volumes are real block devices).
7. Pass `/dev/mapper/ember-vm-myvm` to Firecracker as `path_on_host`.

If `disk_sectors > image size_sectors`, the activation table size already declares the larger virtual size. Ember then runs `e2fsck -f -p` and `resize2fs` against the device to grow the ext4 filesystem into the new space (no `truncate` needed — thin volumes are virtually sized at activation time).

### Sanity check

A `create_snap` completes in milliseconds.
Mirror the macOS/btrfs timing check: warn if the operation takes more than 1 second, since that suggests metadata pressure or pool-level issues.

## VM resize

```bash
ember vm resize myvm --disk-size 8G
```

1. VM must be stopped (existing precondition).
2. Suspend: `dmsetup suspend ember-vm-myvm`.
3. Reload table with new virtual size: `dmsetup load ember-vm-myvm --table "0 <new_sectors> thin /dev/mapper/ember-pool <id_vm>"`.
4. Resume: `dmsetup resume ember-vm-myvm`.
5. `e2fsck -f -p /dev/mapper/ember-vm-myvm`.
6. `resize2fs /dev/mapper/ember-vm-myvm`.

No new blocks are allocated until the guest writes into the new space.
Pool capacity is the upper bound; thin volumes can over-commit it.

Shrinking is not supported (matches every other backend).

## Pool resize

A new admin command:

```bash
ember storage grow --size 100G
```

1. For file-backed: `truncate -s 100G data.img`. For device-backed: assumes the user has already grown the device (e.g. cloud volume expansion).
2. `losetup -c /dev/loopM`: instruct the loop driver to re-read the backing file size. (No-op for device mode.)
3. Suspend: `dmsetup suspend ember-pool`.
4. Reload table: `dmsetup load ember-pool --table "0 <new_data_sectors> thin-pool /dev/loopN /dev/loopM 128 <new_low_water>"`.
5. Resume: `dmsetup resume ember-pool`.

Metadata cannot be resized in place.
If `thin_metadata_size` for the new pool size exceeds the existing metadata device, ember refuses the grow and prints instructions for an offline metadata move using `pdata_tools` (out of scope for the initial implementation; doc only).

## User snapshots

```bash
# Create
ember snapshot create myvm s1
→  id_s1 = fresh_thin_id()
→  dmsetup suspend ember-vm-myvm
→  dmsetup message ember-pool 0 "create_snap <id_s1> <id_vm>"
→  dmsetup resume ember-vm-myvm
   (id_s1 stays inactive — no /dev/mapper entry until restore)

# Restore (VM must be stopped)
ember snapshot restore myvm s1
→  dmsetup remove ember-vm-myvm
→  dmsetup message ember-pool 0 "delete <id_vm>"
→  id_vm_new = fresh_thin_id()
→  dmsetup message ember-pool 0 "create_snap <id_vm_new> <id_s1>"
→  dmsetup create ember-vm-myvm --table "0 <sectors> thin /dev/mapper/ember-pool <id_vm_new>"
   VmMetadata.thin_id = id_vm_new

# List
ember snapshot list myvm
→  read snapshot records from VmMetadata (or a sidecar; see below)

# Delete
ember snapshot delete myvm s1
→  dmsetup message ember-pool 0 "delete <id_s1>"
```

### Snapshot consistency

Suspending the VM volume during `create_snap` flushes outstanding I/O and forces a metadata commit before the snapshot is taken.
The kernel performs the equivalent of an fsync at the block layer.
A guest that has not fsynced its in-flight writes may still see an uncrashed-but-dirty filesystem on the snapshot, exactly as with ZFS zvol snapshots.
This matches existing behavior; no additional guarantees are introduced.

### Snapshot metadata

Snapshot records are stored alongside `VmMetadata`, since the existing ZFS backend reads them via `zfs::snapshot::list`.
For dm-thin, ember maintains a `snapshots: Vec<SnapshotEntry>` list in `vm.json`:

```rust
pub struct SnapshotEntry {
    pub name: String,
    pub thin_id: u64,
    pub created_at: String,
    pub size_sectors: u64,
}
```

`list_snapshots` reads this list.
`size` reflects unique block usage and can be queried via `dmsetup status` or `thin_ls --metadata-snap` for accurate accounting; for the initial implementation, ember reports the volume's virtual size and defers exclusive-block accounting to a future enhancement.

## VM fork

```bash
ember vm fork source newvm
```

`fork` and `clone-for-vm` are the same primitive on dm-thin:

1. Allocate `id_fork = fresh_thin_id()`.
2. Suspend source (if running, this is required for consistency).
3. `dmsetup message ember-pool 0 "create_snap <id_fork> <source.thin_id>"`.
4. Resume source.
5. Activate: `dmsetup create ember-vm-newvm --table "0 <sectors> thin /dev/mapper/ember-pool <id_fork>"`.

Forks are independent of the source after creation — the dm-thin metadata reference-counts blocks, so deleting the source's thin id does not affect the fork.
This mirrors APFS/btrfs behavior, not ZFS:

* `cleanup_fork` is a no-op.
* `storage_dependents` always returns an empty vec.

The `parent_vm` field in `VmMetadata` records the fork origin for informational purposes.

This is a notable simplification compared to the ZFS backend's fork-snapshot dependency tracking.

## Firecracker integration

The drive path is a block device, identical in shape to the ZFS path:

| Backend | `path_on_host` |
|---------|----------------|
| ZFS | `/dev/zvol/tank/ember/vms/myvm` (block device) |
| btrfs | `/var/lib/ember/btrfs/vms/myvm/rootfs.img` (regular file) |
| dm-thin | `/dev/mapper/ember-vm-myvm` (block device) |

`LinuxVm::start` already handles block-device drive paths.
The dispatch logic introduced by the btrfs spec (file path vs ZFS dataset name) extends naturally — dm-thin paths start with `/dev/mapper/`, so they take the file-path branch (passed through unchanged).

The conversion helper that maps a `disk_path` to the actual device path becomes:

```rust
let rootfs_path = if vm.disk_path.starts_with('/') {
    PathBuf::from(&vm.disk_path)  // btrfs file or dm-thin /dev/mapper path
} else {
    zfs::volume::device_path(&vm.disk_path)  // ZFS dataset name
};
```

No further VM-side changes are required.

## VM and image metadata

`VmMetadata` and `ImageEntry` gain a single optional field:

```rust
pub struct VmMetadata {
    // ...
    pub disk_path: String,
    pub parent_vm: Option<String>,
    /// dm-thin volume id. None for ZFS/btrfs/APFS backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thin_id: Option<u64>,
    pub snapshots: Vec<SnapshotEntry>,  // NEW: dm-thin owns this list
    // ...
}

pub struct ImageEntry {
    pub reference: String,
    pub local_name: String,
    pub disk_path: String,
    pub size_mib: u64,
    pub pulled_at: String,
    /// dm-thin base snapshot id. None for ZFS/btrfs/APFS backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thin_id: Option<u64>,
}
```

The `#[serde(skip_serializing_if = "Option::is_none")]` keeps ZFS configs unchanged on disk.
Existing `vm.json` and `registry.json` files are read without modification — the ZFS backend simply ignores `thin_id`.

For ZFS, the `snapshots` list remains empty in `vm.json` and `list_snapshots` continues to read live state from `zfs::snapshot::list`.
The dm-thin backend writes to it.
This split is acceptable but slightly asymmetric; an alternative is for ZFS to mirror its snapshots into `vm.json` too, which is out of scope here.

## Image dependency tracking

With dm-thin, the base thin id can technically be deleted while VMs cloned from it exist — block reference counting at the pool level prevents data loss.
However, ember preserves the user-visible invariant of the existing image registry: `ember image delete` checks for VMs that reference the image and refuses to delete by default, consistent with both ZFS and btrfs.

`destroy_image_storage(name, force)`:

* Without `--force`: refuse if `ImageEntry.thin_id` is referenced by any `VmMetadata.thin_id`'s ancestor chain. Ancestor lookup uses `thin_dump` to walk the snapshot graph.
* With `--force`: delete the thin id directly. Cloned VMs retain their own thin ids and continue to function — block sharing is invisible at the volume level.

## Crate structure

Building on the layout proposed by the btrfs spec:

```
crates/ember-linux/src/
├── storage.rs              # create_storage() factory, returns Arc<dyn StorageBackend>
├── zfs_storage.rs          # ZFS backend (renamed from current storage.rs)
├── btrfs_storage.rs        # btrfs backend
├── dm_thin_storage.rs      # NEW: dm-thin backend
├── zfs/                    # ZFS CLI wrappers (unchanged)
├── btrfs/                  # btrfs CLI wrappers
├── dm_thin/                # NEW: dm-thin CLI wrappers
│   ├── module.rs           # mod declarations
│   ├── pool.rs             # ember-pool create/activate/teardown, status parsing
│   ├── thin.rs             # create_thin, create_snap, delete, suspend/resume, table reload, fresh_thin_id
│   ├── activation.rs       # ensure_pool_active, ensure_thin_active, deactivate
│   └── tools.rs            # thin_check, thin_repair, thin_metadata_size, thin_dump wrappers
├── zvol.rs                 # Existing ext4 → block device pipeline (reused for dm-thin)
└── vm.rs                   # LinuxVm — handles file paths and block device paths
```

`DmThinStorage` mirrors `ZfsStorage` but addresses volumes by id:

```rust
pub struct DmThinStorage {
    /// Backing path (directory for files, raw block dev otherwise).
    storage_path: PathBuf,
    /// Pool block size in sectors. From config.
    block_size: u32,
}
```

The struct holds no allocator state.
`fresh_thin_id()` generates a random `u64` and returns it; collisions are handled by the kernel (`create_thin` returns `EEXIST`) and the caller retries.
The authoritative record of which ids are live lives in `ImageEntry`/`VmMetadata`/`SnapshotEntry`, which are already updated under the existing per-VM and registry locks — no new locking primitive is introduced.

### Display and platform adaptations

`LinuxPlatform` (at `crates/ember-linux/src/platform.rs`) needs the same kind of branching the btrfs spec describes:

* **`inspect_vm_extra`**: "Disk device" / `/dev/mapper/ember-vm-<name>` and "Thin id" / `<id>`.
* **`inspect_image_extra`**: "Disk device" / `/dev/mapper/ember-img-<name>` and "Thin id" / `<id>`.
* **`info_extra`**: "Storage" / "dm-thin", "Pool" / `ember-pool`, "Storage path" / the configured `storage_path`, plus a "Pool usage" line populated from `dmsetup status ember-pool`.
* **`init_hint`**: include the dm-thin variant alongside the ZFS and btrfs hints.

## Comparison: ZFS vs btrfs vs dm-thin vs APFS

| Operation | ZFS (Linux) | btrfs (Linux) | dm-thin (Linux) | APFS (macOS) |
|-----------|-------------|---------------|-----------------|--------------|
| Init | `zpool create` + `zfs create` | `mkfs.btrfs` + `mount` + `mkdir` | `truncate` + `losetup` + `dmsetup create thin-pool` | `mkdir` |
| Base image | zvol + `@base` snapshot | Raw `.img` file | Thin volume + snapshot id | Raw `.img` file |
| VM clone | `zfs clone x@base y` | `cp --reflink=always x.img y.img` | `dmsetup message create_snap` + `dmsetup create` | `cp -c x.img y.img` |
| Snapshot | `zfs snapshot y@snap` | `cp --reflink=always` | suspend + `create_snap` + resume | `cp -c` |
| Restore | `zfs rollback y@snap` | `cp --reflink=always` + `mv` | remove + delete + `create_snap` + create | `cp -c` + `mv` |
| Delete snap | `zfs destroy y@snap` | `rm snap.img` | `dmsetup message delete` | `rm snap.img` |
| Resize | `zfs set volsize` + `resize2fs` | `truncate` + `resize2fs` | `dmsetup load` + `resize2fs` | `truncate` + `resize2fs` |
| Fork | `zfs clone` (creates dependency) | `cp --reflink=always` (independent) | `create_snap` (independent) | `cp -c` (independent) |
| Drive path | `/dev/zvol/...` | `.../rootfs.img` (file) | `/dev/mapper/...` | `.../rootfs.img` (file) |
| Root required | Yes | Yes | Yes | No |
| Filesystem validation | `zpool list` | `/proc/mounts` | `dmsetup status ember-pool` | APFS volume check at init |
| Reactivation after reboot | Auto (zpool import) | Auto-mount | Explicit `ensure_pool_active` | Not applicable |
| Identifier | Dataset path | File path | Random 24-bit thin id | File path |
| State on disk | ZFS metadata | Filesystem metadata | Pool metadata (ids embedded in existing vm/image records) | Filesystem metadata |
| Kernel module | Out-of-tree (DKMS) | In-tree | In-tree | N/A |
| Checksums | Yes (ZFS) | Yes (data + metadata) | Metadata only | No |

dm-thin sits between ZFS and btrfs:
it offers ZFS-like block-level CoW with no kernel module, at the cost of a more involved activation lifecycle (numeric ids, explicit `dmsetup` operations, no auto-import) and weaker data-integrity guarantees (no data checksums, harsher pool-exhaustion failure mode).

## Storage efficiency diagnostics

`ember debug storage-efficiency` for dm-thin reports both per-volume and pool-level metrics:

* Per-volume virtual size: from the activated device's table.
* Per-volume exclusive blocks: from `thin_ls --metadata-snap=- /dev/loopMETA`. Computing this requires a metadata snapshot — taken under suspend or via `dmsetup message ember-pool 0 "reserve_metadata_snap"` — which has measurable overhead. The command surfaces it on demand only.
* Pool capacity, allocated, and free: from `dmsetup status ember-pool`. Output format: `<used_data>/<total_data> <used_metadata>/<total_metadata>`.

The macOS `st_blocks` approach used by the btrfs and APFS backends does not apply — dm-thin volumes are block devices, not files, and `stat` on `/dev/mapper/...` reports no allocation.

## Risks and limitations

* **Pool exhaustion**: Sparse-file backing lets the pool over-commit. If the host filesystem fills up, the pool transitions to read-only and all thin volumes return EIO until space is recovered. Ember should pre-check available space on the host filesystem before allowing image pulls or VM creates that would push the pool toward its data limit. The initial implementation adds a refuse-on-pool-full check via `dmsetup status` before each write-heavy operation; richer monitoring is a follow-up.
* **Metadata exhaustion**: Less recoverable than data exhaustion. The metadata device must be sized generously at init. `ember storage info` should warn when metadata usage exceeds 80%.
* **Block size is permanent**: Chosen at `dmsetup create`; cannot be changed without rebuilding the pool. The 64 KiB default is a balance; users with very large VM disks (~hundreds of GiB) may want 128–256 KiB blocks for lower metadata overhead.
* **Loop device limits**: The default `max_loop=8` per kernel module load can be a constraint on systems with many loop-using services. Ember uses two loop devices total (metadata and data); the limit only matters when other software is competing. Documented as a troubleshooting hint, not a hard requirement.
* **Numeric id lifecycle**: Thin ids live on `VmMetadata`/`ImageEntry`/`SnapshotEntry`. Loss of the state directory therefore loses the name→id map even though the pool metadata is intact. Recovery is possible via `thin_dump` (lists all live thin ids) but requires manual reconstruction. No worse than the equivalent loss for ZFS or btrfs configs.
* **Concurrent invocations**: Race-free by construction. The kernel rejects duplicate ids atomically; the random-pick-and-retry loop tolerates concurrent creators without coordination. Per-record state mutation (writing `thin_id` into `vm.json` etc.) is already serialized by the existing per-VM and registry locks.
* **No data checksums**: Bit rot on the underlying block device goes undetected. Users who need this should layer dm-thin on top of LVM mirrors or hardware RAID, or stay on ZFS.
* **No `send`/`receive` equivalent**: Backup and migration require `dd` of the activated device, or `thin_dump` + `thin_delta` for incremental sync. Out of scope for the initial implementation.

## External dependencies

* **`dmsetup`**: From the `lvm2` package on Debian/Ubuntu/RHEL/Fedora/Arch. Installed by default on most server distributions.
* **`losetup`**: From `util-linux`. Always present.
* **`thin-provisioning-tools`**: Provides `thin_check`, `thin_repair`, `thin_dump`, `thin_metadata_size`, `thin_ls`. Packaged separately on most distributions. Required by `ember init` and `ember storage info`. Pre-flight check at `ember init` time.
* **`e2fsprogs`**: `mkfs.ext4`, `e2fsck`, `resize2fs`. Already required by the ZFS backend.
* **GNU coreutils**: `truncate`, `dd`. Already required.
* **Kernel config**: `CONFIG_DM_THIN_PROVISIONING=y` or `=m`, `CONFIG_BLK_DEV_LOOP=y` or `=m`. Both are part of every mainstream distribution kernel.

## Open questions

* **Multi-instance support**: The current spec hardcodes the pool name `ember-pool` and the device-mapper prefixes. Running multiple independent ember installations on the same host requires per-instance prefixes. Deferred until a real use case appears.
* **Metadata on a separate device**: `ember init --metadata-device /dev/sdc1` could place metadata on faster storage (NVMe) while data lives on bulk storage (HDD). Easy to add later — the pool table already supports two distinct devices.
* **Discard/TRIM**: dm-thin supports passdown of discards from guest to pool, which can return blocks to the pool when guests TRIM. Requires Firecracker virtio-blk to advertise discard support and the guest filesystem to issue it. Worth investigating as a follow-up; not required for correctness.
* **`dmeventd` integration**: Userspace handler for low-water-mark events would let ember warn proactively. The initial implementation polls `dmsetup status` on demand instead.
