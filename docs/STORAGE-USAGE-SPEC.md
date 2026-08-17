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

Four numbers per volume, all in bytes.

| Field | Meaning |
|-------|---------|
| `provisioned` | Virtual size the guest sees. What we report today. |
| `exclusive` | Physical bytes only this volume references. What a destroy returns to the pool. |
| `referenced` | Physical bytes reachable from this volume, shared blocks included. |
| `logical` | Uncompressed size of `referenced`. |

`exclusive` is the number every backend can produce and the one users act on.
`referenced` and `logical` are optional because not every backend can measure them.

Two useful quantities are derived rather than stored, so they cannot disagree with their inputs:

* Shared bytes: `referenced - exclusive`.
* Compression ratio: `logical / referenced`.

### Types

In `ember-core/src/backend.rs`, next to `VolumeHandle`:

```rust
/// Space accounting for a single volume, in bytes.
///
/// Backends fill in what they can measure. `exclusive` is the one
/// field every backend produces, and the one a user acts on: it is
/// what destroying the volume actually returns to the pool.
pub struct VolumeUsage {
    pub provisioned: u64,
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
A per-volume `vm_usage(&VmMetadata)` would make dm-thin reserve a metadata snapshot and walk the mapping trees once per VM.

No default implementation.
A new backend must decide what it can measure rather than silently inheriting zeros.

## Backend mappings

### ZFS

Everything comes from two commands against `<pool>/<dataset>`.

Volumes, one call covering both the `images/` and `vms/` subtrees:

```
zfs list -Hp -r -t volume -o name,volsize,usedbydataset,usedbysnapshots,referenced,logicalreferenced <base>
```

| Field | ZFS property |
|-------|--------------|
| `provisioned` | `volsize` |
| `exclusive` | `usedbydataset + usedbysnapshots` |
| `referenced` | `referenced` |
| `logical` | `logicalreferenced` |

Deliberately not the `used` property, which is the obvious choice and the wrong one.
A zvol created by `zfs create -V` carries a refreservation for its full virtual size, and `used` counts that reservation as consumed space.
Image volumes on a live pool therefore report a `used` of 8.4 GiB against a `referenced` of 1.9 GiB, which would put exclusive above referenced and make the shared column meaningless.
Clones carry no reservation, so the two definitions agree for VMs and diverge only for images.

Rows are matched to records by dataset name, which for ZFS is what `VmMetadata::disk_path` and `ImageEntry::disk_path` already hold.

`-p` gives exact byte counts. We do not read `refcompressratio`, since `logicalreferenced / referenced` reproduces it and cannot drift from the other fields.

Pool, one call:

```
zfs get -Hp -o value used,available,logicalused <base>
```

`capacity` is `used + available` and `allocated` is `used`.
This deliberately describes the dataset tree ember owns rather than the raw vdev, so quotas and sibling datasets on a shared pool are accounted for and the free figure means what a user expects.

Note that pool `allocated` does include refreservations while per-volume `exclusive` does not, so the volume rows do not sum exactly to the pool line.
That gap is space genuinely charged to the tree, not a rounding artifact.

`metadata` is `None`.

### dm-thin

Pool numbers are already parsed.
`pool::status` returns `PoolStatus` in blocks, so this is arithmetic on values we fetch today only to gate on health:

* `capacity` = `total_data_blocks` × pool block size
* `allocated` = `used_data_blocks` × pool block size
* `metadata` = `total_metadata_blocks` and `used_metadata_blocks`, each × 4096, the fixed thin-pool metadata block size
* `logical` = `None`

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
Volumes with no `thin_id` recorded are omitted from the map rather than reported as zero.

This path works for volumes that are not currently activated, which matters because dm-thin activates lazily and a stopped VM usually has no `/dev/mapper` entry. Reading `dmsetup status` on the thin device instead would only cover active volumes and would give mapped sectors without an exclusive count.

Three hazards to handle:

* **The snapshot is a single slot per pool.** `reserve_metadata_snap` fails with `EBUSY` when one is already held. Report that as a distinct error naming `dmsetup message <pool> 0 release_metadata_snap` as the remedy, because the usual cause is a stale reservation from a killed process, and a stale reservation also pins metadata blocks that the pool would otherwise reuse.
* **Release must happen on every path.** The release is done by a guard type whose `Drop` fires on early return and on panic, not by a trailing statement.
* **We never force-release.** A reservation we did not take may belong to another process. We fail with the message above instead of stealing it.

### APFS

* `provisioned` is the disk image file length.
* `exclusive` is `st_blocks` × 512, which on APFS counts only blocks not shared with a clone. This is the same measurement `debug storage-efficiency` uses today.
* `referenced` and `logical` are `None`.

Pool numbers mirror the ZFS treatment so the two read the same way: `allocated` is the sum of volume `exclusive` values, and `capacity` is that sum plus the containing filesystem's available space.

## CLI surface

### `ember storage usage`

New subcommand next to `ember storage grow`.

```
$ ember storage usage

Pool          481.4 GiB capacity, 298.6 GiB used (62%), 182.8 GiB free
Compression   599.4 GiB logical -> 298.6 GiB on disk (2.01x)

NAME                  PROVISIONED   REFERENCED   EXCLUSIVE   SHARED   RATIO
aj-dev                    200 GiB     98.5 GiB    97.2 GiB  1.3 GiB   1.97x
mz-dev                    200 GiB     10.1 GiB     8.2 GiB  1.9 GiB   2.08x
mz-dev-auto-scaling       200 GiB     93.6 GiB    90.8 GiB  2.8 GiB   2.11x
mz-dev-bugs               200 GiB     88.6 GiB    85.3 GiB  3.4 GiB   2.23x

IMAGES
ubuntu-dev                  8 GiB      1.9 GiB     1.9 GiB      0 B   2.25x
ubuntu-dev-new              8 GiB      2.0 GiB     2.0 GiB      0 B   2.27x
```

Columns whose backing field is `None` render `-`.
On dm-thin that means the `RATIO` column is `-` throughout and the `Compression` line is omitted, and a `Metadata` line appears instead showing metadata device usage.

`--format json` emits `StorageUsage` directly, matching the `OutputFormat` enum the other commands use.

This command reports backend errors as errors. It is the one place where being unable to measure is a failure rather than a blank.

### `ember vm list`

Gains a `USED` column showing `exclusive`, next to the existing provisioned `DISK`.

Usage here is best-effort. If `usage()` fails, every row renders `-` and the listing still succeeds. Listing VMs must not start depending on a healthy pool, since one common reason to list them is that storage is broken.

### `ember vm inspect`

Gains `Used`, `Referenced`, `Shared`, and `Ratio` rows, omitting the ones whose field is `None`. Best-effort, same rule as `vm list`.

### `ember info`

Gains a pool capacity line, and a compression line when `PoolUsage::logical` is present. Best-effort.

### Removed

`ember debug storage-efficiency` is deleted, and with it the `debug` subcommand tree, which has no other members. Its useful half is `ember storage usage` and its APFS-specific half is now `MacosStorage::usage`.

## Testing

Unit tests, no root required:

* `thin_ls` output parsing, including the empty-pool case, unparseable rows, and ids present in the pool that no record claims.
* `zfs list -Hp` row parsing, including volumes not in the state store.
* Metadata block accounting from a `PoolStatus`.
* Derived shared bytes and ratio, including the divide-by-zero guard when `referenced` is 0.
* Rendering of `-` for every `None` field.

Integration tests in `tests/storage_usage.rs`, using the existing `TestEnv`:

* After `ember vm create`, `ember storage usage` lists the VM with `exclusive > 0` and `referenced >= exclusive`.
* After a fork, the fork's `referenced` exceeds its `exclusive`, which is the sharing the CoW backends are supposed to deliver.
* `ember vm list` still succeeds and renders `-` when the backend cannot report.
* The dm-thin variant runs behind the existing `--ignored` root gate, and asserts that the metadata snapshot is released by reserving one again afterwards.

## Out of scope

* Per-snapshot accounting. Snapshots are visible in the ZFS numbers via `used` but get no rows of their own.
* Historical tracking or deltas over time.
* Any accounting for a compression layer that does not exist yet. When one is added, its physical-versus-logical figures land in `PoolUsage::logical` and `VolumeUsage::logical`, which are already shaped for it.
