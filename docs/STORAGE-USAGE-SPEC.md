# Ember — Storage Usage Accounting

Ember reports provisioned sizes everywhere and actual sizes nowhere.
`ember vm list` prints `VmMetadata.disk_size_gib`, `ember vm inspect` prints the same field, and `ember info` prints no capacity at all.
The only command that tries, `ember debug storage-efficiency`, walks `state_dir/images/data/*.img` and `state_dir/vms/<name>/rootfs.img`, which are paths that exist only on macOS.
On Linux it reports zero for everything.

This spec adds one accounting method to `StorageBackend` and wires four commands to it.

## What we want to be able to answer

* How much disk does this VM actually occupy, and how much comes back if I delete it?
* How much is shared with the image or fork origin, and how much has diverged?
* How full is the pool, and how much of the saving is compression?

The third question is also the measuring instrument for the compression work described in `DM-THIN-SPEC.md`.
Without it we cannot say what a compression layer would buy, nor whether it helped after enabling it.

## The model

This is an **occupancy** model. It answers where space has gone, not what a delete would give back.
Those are different questions on ZFS, and trying to make one field answer both is what makes the numbers incoherent.

Four numbers per volume, all in bytes.

| Field | Meaning |
|-------|---------|
| `provisioned` | Virtual size the guest sees. What we report today. |
| `exclusive` | Physical bytes this volume holds that are not shared with an origin. |
| `referenced` | Physical bytes reachable from this volume, shared blocks included. |
| `logical` | Uncompressed size of `referenced`. |

`exclusive` is the number every backend can produce.
`referenced` and `logical` are optional because not every backend can measure them.

The invariant that keeps the table readable is `exclusive <= referenced` whenever `referenced` is known.
Any definition of `exclusive` that can exceed what the volume references makes the derived shared column meaningless.

Two quantities are derived rather than stored, so they cannot disagree with their inputs:

* Shared bytes: `referenced - exclusive`.
* Compression ratio: `logical / referenced`.

### What `exclusive` is not

It is not what a destroy frees. On ZFS a volume is additionally charged for its refreservation and for blocks held only by its own snapshots, and neither is in `exclusive`.
Nor does it capture the origin side of a clone relationship: ZFS charges blocks shared between an origin and its clones to the origin, so an image whose clones all still exist reports occupancy for blocks that no single delete can reclaim.
dm-thin refcounts symmetrically and has neither problem.

We accept the asymmetry rather than paper over it. Reclaim accounting on ZFS depends on the whole clone graph, and the pool line already tells a user how much room is left.

### Types

In `ember-core/src/backend.rs`, next to `VolumeHandle`:

```rust
/// Space accounting for a single volume, in bytes.
pub struct VolumeUsage {
    pub provisioned: u64,
    /// Physical bytes this volume holds that are not shared with an
    /// origin. Always within `referenced` when that is known.
    pub exclusive: u64,
    /// `None` when the backend cannot separate shared blocks from
    /// exclusive ones.
    pub referenced: Option<u64>,
    /// Uncompressed size of `referenced`. `None` when the backend does
    /// not compress.
    pub logical: Option<u64>,
}

/// Pool-wide capacity, in bytes.
pub struct PoolUsage {
    pub capacity: u64,
    pub allocated: u64,
    /// Part of `allocated` that is reserved but holds no data, so it
    /// compresses to nothing and stays out of the ratio. Zero for
    /// backends without reservations.
    pub reserved: u64,
    /// Uncompressed size of the data within `allocated`. `None` when
    /// the backend does not compress.
    pub logical: Option<u64>,
    /// Backends with a separate metadata device report it here.
    pub metadata: Option<MetadataUsage>,
}

pub struct MetadataUsage {
    pub capacity: u64,
    pub used: u64,
}

/// Accounting for a whole installation, produced in one pass.
pub struct StorageUsage {
    pub pool: PoolUsage,
    /// Keyed by `VmMetadata::name`. A missing key means the backend
    /// could not account for that VM.
    pub vms: BTreeMap<String, VolumeUsage>,
    /// Keyed by `ImageEntry::local_name`.
    pub images: BTreeMap<String, VolumeUsage>,
}
```

### Trait surface

One method, not three:

```rust
/// Measure space usage across the installation.
///
/// Takes the state records rather than discovering volumes itself,
/// because the name-to-volume mapping lives in state and not in the
/// backend. Returns the whole set in one value so that backends which
/// have to walk pool-wide metadata do that walk once instead of once
/// per volume.
fn usage(&self, vms: &[VmMetadata], images: &[ImageEntry]) -> Result<StorageUsage>;
```

The batching is the point.
A per-volume `vm_usage(&VmMetadata)` would make dm-thin reserve a metadata snapshot and walk the mapping trees once per VM, and it could not express the APFS answer at all, where a volume's exclusive figure is only defined relative to every other volume that might share its blocks.

No default implementation.
A new backend must decide what it can measure rather than silently inheriting zeros.

## Backend mappings

### ZFS

Everything comes from two commands against `<pool>/<dataset>`.

Volumes, one call covering both the `images/` and `vms/` subtrees:

```
zfs list -Hp -r -t volume -o name,volsize,usedbydataset,usedbyrefreservation,referenced,logicalreferenced <base>
```

| Field | ZFS property |
|-------|--------------|
| `provisioned` | `volsize` |
| `exclusive` | `usedbydataset` |
| `referenced` | `referenced` |
| `logical` | `logicalreferenced` |

Two neighbouring properties look like better fits for `exclusive` and both break the invariant.

`used` is the obvious choice, and it is what a destroy frees, but a zvol from `zfs create -V` carries a refreservation for its full virtual size and `used` counts that reservation as consumed.
Image volumes on a live pool report a `used` of 8.4 GiB against a `referenced` of 1.9 GiB.

Adding `usedbysnapshots` fails for a subtler reason: that property is by definition space the live volume no longer references, so folding it in pushes occupancy outside `referenced`.
On a pool whose fork snapshots hold a single kilobyte, that is enough to make both image rows report `exclusive` above `referenced`.

`usedbydataset` is a subset of `referenced` by ZFS's own definition, so the invariant holds by construction.

Rows are matched to records by dataset name, which for ZFS is what `VmMetadata::disk_path` and `ImageEntry::disk_path` already hold.

`-p` gives exact byte counts. We do not read `refcompressratio`, since `logicalreferenced / referenced` reproduces it and cannot drift from the other fields.

Pool, one call:

```
zfs get -Hp -o value used,available,logicalused <base>
```

`capacity` is `used + available` and `allocated` is `used`.
This deliberately describes the dataset tree ember owns rather than the raw vdev, so quotas and sibling datasets on a shared pool are accounted for and the free figure means what a user expects.

`reserved` is the sum of `usedbyrefreservation` over the volume rows.
ZFS exposes no aggregate property for it, which is why it comes from the same listing rather than a third call.
It matters because `used` includes reservations and `logicalused` does not, so dividing one by the other counts empty reservation as perfectly compressed data.
On a live pool that understates compression by about 5%, 2.01x against a true 2.11x, and this report is meant to be the instrument we judge a compression layer by.

The reservation is also why the volume rows do not sum to the pool line, so the CLI prints it as its own row rather than leaving a silent gap.

`metadata` is `None`.

### dm-thin

Unlike every other method on the backend, `usage` does not activate the pool.
Measuring is a query, and `ember vm list` has no business loading a pool table, attaching loop devices, and running a full `thin_check` as a side effect of listing VMs.
An inactive pool produces an error saying so, which the best-effort callers render as `-`.

Pool numbers are already parsed.
`pool::status` returns `PoolStatus` in blocks, so this is arithmetic on values we fetch today only to gate on health:

* `capacity` = `total_data_blocks` × pool block size
* `allocated` = `used_data_blocks` × pool block size
* `metadata` = `total_metadata_blocks` and `used_metadata_blocks`, each × 4096, the fixed thin-pool metadata block size
* `reserved` = 0, since a thin volume is never charged for space it has not written
* `logical` = `None`

Note the two different multipliers. Data is counted in pool blocks (64 KiB by default) and metadata in the kernel's fixed 4 KiB blocks, so using one scale for both misreports metadata by 16x.

Per-volume numbers need a metadata snapshot, because the live metadata device is owned by the kernel and cannot be read directly:

1. `dmsetup message <pool> 0 reserve_metadata_snap`
2. `thin_ls -m --no-headers -o DEV,MAPPED_BYTES,EXCLUSIVE_BYTES <metadata_loop_dev>`
3. `dmsetup message <pool> 0 release_metadata_snap`

| Field | Source |
|-------|--------|
| `provisioned` | `disk_size_gib` / `size_mib` from the record |
| `exclusive` | `EXCLUSIVE_BYTES` |
| `referenced` | `MAPPED_BYTES` |
| `logical` | `None` |

Rows are matched to records by thin id.
A record with no `thin_id`, and a `thin_id` the pool no longer knows about, are both omitted from the map rather than reported as zero.
Ids the pool holds that no record claims (a staging volume leaked by a failed image pull, another install) are ignored, so they show up in the pool figure without inventing a row.

This path works for volumes that are not currently activated, which matters because dm-thin activates lazily and a stopped VM usually has no `/dev/mapper` entry. Reading `dmsetup status` on the thin device instead would only cover active volumes and would give mapped sectors without an exclusive count.

Four hazards to handle:

* **The snapshot is a single slot per pool.** `reserve_metadata_snap` fails with `EBUSY` when one is already held. Report that as a distinct error naming `dmsetup message <pool> 0 release_metadata_snap` as the remedy, because the usual cause is a stale reservation from a killed process, and a stale reservation also pins metadata blocks that the pool would otherwise reuse.
* **Release must happen on every path.** The release is done by a guard type whose `Drop` fires on early return and on panic, not by a trailing statement.
* **A signal still leaks it.** `Drop` does not run on SIGINT, so Ctrl-C during a `thin_ls` scan strands a reservation and the next reader sees the EBUSY above. We accept that and make the error tell the operator how to clear it, rather than installing a signal handler for one command.
* **We never force-release.** A reservation we did not take may belong to another process. We fail with the message above instead of stealing it.

When the reservation or the scan fails, the whole call fails, even though the pool figures were already in hand. `ember storage usage` exists to measure, so a partial answer that looks complete is worse than an error.

### APFS

Sharing between APFS clones is visible from userspace. `fcntl(F_LOG2PHYS_EXT)` maps a logical offset in a file to the physical byte range backing it, and `SEEK_DATA` skips holes without walking them. Reading a file's extents tells us which physical bytes it maps, and two files that map the same bytes are sharing them.

`st_blocks` cannot answer this and must not be used for it. It counts the blocks a file maps, not the blocks it owns, so a fresh clone reports its origin's full figure while costing nothing. It separates allocated extents from holes, not shared blocks from unshared ones. Summing it over an install of one image and fifteen clones overstates real occupancy by more than 5x, and feeding that sum into `capacity` makes the reported capacity grow every time a free clone is made.

So we scan rather than stat:

1. Walk `vms/` and `images/` for every `.img` file.
2. Read each one's physical extents.
3. Sweep all the extents together. A physical byte mapped by exactly one file is exclusive to that file, a byte mapped by several is shared, and the count of distinct bytes mapped is the tree's real occupancy.

| Field | Source |
|-------|--------|
| `provisioned` | file length |
| `exclusive` | bytes in extents no other file in the tree maps |
| `referenced` | sum of the file's extent lengths, which equals `st_blocks` × 512 |
| `logical` | `None`, ember does not compress its disk images |

`exclusive <= referenced` holds by construction, since the exclusive bytes are a subset of the file's own extents.

The whole tree is scanned even when the caller passes a single record, and two separate things depend on that. The pool figure is installation-wide while a caller such as `vm inspect` hands the backend one VM. And exclusivity is not a property of a volume on its own: a file's blocks are exclusive only relative to everything else that might map them, so a sweep restricted to the requested records would report every volume as fully exclusive.

That is the second reason the trait batches. The dm-thin argument is about doing one expensive walk instead of many. On APFS, batching is what makes the answer computable at all.

Pool numbers mirror the ZFS treatment so the two read the same way. `allocated` is the union of every extent in the tree, each shared byte counted once, and `capacity` is that plus the containing filesystem's available space. `reserved` is 0 and `metadata` is `None`.

Two limits we state rather than chase:

* Exclusive means "not shared with another volume ember owns". Blocks shared with a file outside the tree, a user's own `cp -c` of a rootfs for instance, are counted as exclusive. This is the same species of gap as the ZFS asymmetry above.
* As on ZFS, `exclusive` is not what a delete frees. An APFS snapshot can hold the blocks after the file is gone.

The cost is a full extent scan per report. APFS coalesces aggressively, so extent counts track the number of data islands rather than file size: 4 GiB of contiguous data is around 130 extents, while a sparse ext4 rootfs with metadata scattered through it runs a few thousand. A tree of one image, two VMs and three forks scans in about 25 ms. Should that ever become a problem, the cheap path is available without changing the model, since `vm list` and `info` are best-effort and can stat for `referenced` alone and leave the sweep to `ember storage usage`.

## CLI surface

### `ember storage usage`

New subcommand next to `ember storage grow`.

Captured from a live ZFS pool:

```
$ ember storage usage

Pool          481.4 GiB capacity, 297.5 GiB used (62%), 183.9 GiB free
Compression   599.3 GiB logical -> 284.3 GiB on disk (2.11x)
Reserved      13.2 GiB charged to the pool but holding no data

VMS
NAME                PROVISIONED REFERENCED EXCLUSIVE  SHARED COMPRESSION
aj-dev                  200 GiB   98.5 GiB  97.2 GiB 1.3 GiB       1.97x
mz-dev                  200 GiB   10.1 GiB   8.2 GiB 1.9 GiB       2.08x
mz-dev-auto-scaling     200 GiB   92.5 GiB  89.7 GiB 2.8 GiB       2.13x
mz-dev-bugs             200 GiB   88.7 GiB  85.3 GiB 3.4 GiB       2.22x

IMAGES
NAME           PROVISIONED REFERENCED EXCLUSIVE SHARED COMPRESSION
ubuntu-dev         6.3 GiB    1.9 GiB   1.9 GiB    0 B       2.24x
ubuntu-dev-new     6.7 GiB      2 GiB     2 GiB    0 B       2.26x
```

Columns whose backing field is `None` render `-`.
On dm-thin that means the `COMPRESSION` column is `-` throughout and the `Compression` line is omitted, and a `Metadata` line appears instead showing metadata device usage.
The `Reserved` line appears only when a backend has reservations, so only on ZFS.

`--format json` emits `StorageUsage` directly, matching the `OutputFormat` enum the other commands use.

This command reports backend errors as errors. It is the one place where being unable to measure is a failure rather than a blank.

### `ember vm list`

Gains a `USED` column showing `exclusive`, next to the existing provisioned `DISK`.

Usage here is best-effort. If `usage()` fails, every row renders `-` and the listing still succeeds. Listing VMs must not start depending on a healthy pool, since one common reason to list them is that storage is broken.

### `ember vm inspect`

Gains `Used`, `Referenced`, `Shared`, and `Compression` rows, omitting the ones whose field is `None`. Best-effort, same rule as `vm list`.

### `ember info`

Gains a `Capacity` line, and a compression line when the backend compresses. Best-effort.
Not labelled `Pool`, because `info_extra` already prints `ZFS pool <name>` and two unrelated rows called pool read as a contradiction.

### Removed

`ember debug storage-efficiency` is deleted, and with it the `debug` subcommand tree, which has no other members. Its useful half is `ember storage usage` and its APFS-specific half is now `MacosStorage::usage`.

References in `README.md`, `MACOS-SPEC.md`, `MACOS-TODO.md`, `BTRFS-SPEC.md`, `DM-THIN-SPEC.md`, and `TEST-SPEC.md` are updated to the new command, and `tests/macos_storage.rs` is retargeted at it.

### Where the best-effort helper lives

`try_usage` sits in `src/backend.rs`, alongside `create_storage`, not in the `storage` subcommand module.
It is not a `storage` subcommand concern, it is how non-storage commands ask for usage, and putting it under `cli::storage` would make `cli::vm` and `cli::storage` import each other.

It builds the backend through `try_create_storage`, the fallible sibling of `create_storage`. The infallible form panics on a config naming an unimplemented backend, and a panic is not best-effort: `vm list` and `info` are exactly the commands someone runs to diagnose a bad config.

## Testing

Unit tests, no root required. Note that the workspace sets `default-members = ["."]`, so a bare `cargo test` runs only the root package. The backend crates need naming: `cargo test -p ember-core -p ember-macos` on a Mac, and `-p ember-linux` on Linux, where the ext4 helpers those tests shell out to actually exist.

* `thin_ls` output parsing: the empty pool, a header row that `--no-headers` failed to suppress, and short rows.
* `zfs list -Hp` row parsing, including the `-` ZFS prints for inapplicable properties.
* Occupancy never exceeding `referenced`, checked against the four rows a live pool actually produces rather than a hand-written fixture. An earlier cut passed its own guard test because the fixture zeroed the one field that broke the invariant.
* Metadata block accounting from a `PoolStatus`, pinning that data and metadata use different multipliers.
* The thin-id join: a record matched to its row, a record with no id, a record with a stale id, and a row no record claims.
* Derived shared bytes and compression ratio, including the divide-by-zero guards and the saturating subtraction.
* Rendering of `-` for every `None` field.
* The APFS extent sweep, as pure interval logic over synthetic extents so it needs no APFS to run: pristine clones share everything and hold nothing exclusively, a partly rewritten clone holds exactly what it rewrote, a byte shared three ways is exclusive to none of them, and the union counts each shared byte once.

Integration tests in `tests/storage_usage.rs`, using the existing `TestEnv`. All of them are `#[ignore]`d, matching every other file in `tests/`, because `TestEnv` builds a real backend and needs root on Linux. `run-integration-tests.sh` passes `--ignored`, so a test left un-ignored would be skipped by the project runner and would break a bare `cargo test`.

* After `ember vm create`, `ember storage usage` lists the VM with `exclusive > 0` and `exclusive <= referenced`.
* The image row satisfies the same invariant, which is where the refreservation trap bites.
* After a fork, the fork's `referenced` exceeds its `exclusive`, which is the sharing every CoW backend is supposed to deliver, APFS included. Forks are created with `--no-start`, since `TestEnv` installs a kernel that cannot boot.
* After a fork, the pool's `allocated` is below the sum of the volumes' `referenced`. This is the property that fails when a backend counts a shared block once per clone, and it is checked against a real fork rather than a fixture, because the assumption that broke here was one no fixture would have questioned. macOS-only for now: the same should hold on ZFS, but `allocated` there also carries refreservation and snapshot charges, so the honest assertion is a different one and belongs with someone who can run it against a pool.
* `ember vm list` still succeeds and its row ends in `-` when the backend cannot report. Linux-only: the break is a `config.json` naming a nonexistent pool, and the APFS backend reads neither `pool` nor `storage_path`.
* The dm-thin variant creates an image and a VM so the thin-id join is actually exercised, then calls the command a third time to prove the metadata snapshot was released.

## Out of scope

* Per-snapshot accounting. Snapshots are visible in the ZFS numbers via `used` but get no rows of their own.
* Historical tracking or deltas over time.
* Any accounting for a compression layer that does not exist yet. When one is added, its physical-versus-logical figures land in `PoolUsage::logical` and `VolumeUsage::logical`, which are already shaped for it.
