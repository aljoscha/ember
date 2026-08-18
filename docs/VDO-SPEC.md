# Ember — dm-vdo Compression Layer

Transparent compression (and optional deduplication) beneath the dm-thin
pool, enabled with `ember init --storage dm-thin --vdo`.

dm-thin stores blocks verbatim. ZFS, the other Linux backend, compresses
by default and delivers a little over 2x on real ember pools. This spec
closes that gap for dm-thin by inserting a `dm-vdo` target between the
pool's data device and its backing store.

VDO is in-tree since Linux 6.9 (`drivers/md/dm-vdo`). It is opt-in, off
by default, and confined to the dm-thin backend. ZFS and APFS are
untouched.

## Where VDO sits

```text
data.img (sparse file)          raw block device
        |                              |
   /dev/loopN                          |
        \_____________  _______________/
                      \/
        /dev/mapper/ember-{ns}-vdo      <- compression happens here
                      |
            thin-pool data device
                      |
        thin volumes (ember-{ns}-vm-*, ember-{ns}-img-*)
```

The pool's **metadata** device stays on a plain loop device and does not
go through VDO. Thin metadata is small, random-write heavy, and latency
critical, and the offline tools (`thin_check`, `thin_ls`, `thin_repair`)
read that device directly. Routing it through a compressing layer would
cost latency, buy almost nothing (btree nodes compress poorly), and add
an activation ordering hazard for no reason.

Nothing above the data device changes. Thin volumes, thin ids,
snapshots, and forks work exactly as they do today, because to the pool
its data device is still just a block device.

## Sizing

VDO introduces a third level of over-provisioning, so it is worth being
precise about which number means what.

| Level | Name here | Meaning |
| --- | --- | --- |
| 1 | volume virtual size | what a guest sees, sum may exceed the pool |
| 2 | VDO logical / pool data capacity | what the pool will hand out |
| 3 | VDO physical / backing size | real bytes on the disk |

`--size` keeps its current meaning: **the real disk budget**, level 3.
That is the number the operator actually has to buy, and changing what
it means when a flag is added would be a trap.

`--vdo-logical-size` sets level 2 and **defaults to `--size`**, so the
default configuration does not deliberately over-provision. With a 300G
pool you still get 300G of thin-pool data capacity, and compression
shows up as the sparse `data.img` consuming perhaps 150G of real disk
instead of 300G. That is the whole benefit for a pool sharing a
filesystem with other things.

Over-provisioning is available (`--vdo-logical-size 600G`) but it is an
explicit, informed choice, because it makes the failure mode two
sections down far easier to reach.

### VDO's own overhead, and why 1:1 is still a small bet

A VDO volume reserves at least 3 GB for metadata and its deduplication
index, more with a larger index or physical size, and that reserve is
charged to physical usage. So even at 1:1 the pool promises slightly
more than it can store: it hands out `size` while VDO holds at most
`size - overhead` of compressed data. The pool stays safe as long as
compression clears `size / (size - overhead)`.

That break-even is what sets the floor. `ember init --vdo` refuses a
physical size below `vdo::MIN_PHYSICAL_BYTES` (32 GiB), where the
reserve is roughly a tenth of the volume and break-even is about 1.1x,
which any real filesystem beats. At 8 GiB it would be nearer 1.6x, which
is a bet rather than a default.

A freshly created pool therefore reports several gigabytes used and no
ratio at all. Both are honest: the reserve is space the pool cannot
touch, and a pool holding no data has no ratio to report. The reserve is
reported separately as `reserved` so it stays out of the ratio once
there is data, which the `usage` section below covers.

### Deduplication index

The index is sized automatically from the physical size, one GB of index
memory per TB of physical space (the density of a dense UDS index),
rounded up to a value `vdoformat` accepts and floored at its smallest
setting. So the deduplication window always covers the whole pool and
there is no knob to get wrong.

The slab size is pinned explicitly at 2 GiB rather than inherited from
`vdoformat`'s default, because physical space grows one slab at a time
and the growth check has to know the number.

`vdoformat` always writes the index region, whether or not deduplication
is later switched on, so the disk cost is paid either way. Only the RAM
cost (roughly the index memory size) is avoided by leaving it off.

## Compression and deduplication defaults

Compression is **on**. It is the reason this layer exists, and the
kernel's own default is off, so the table always carries `compression
on`.

Deduplication is **off** unless `--vdo-dedup` is passed. Two reasons.
It costs RAM proportional to the pool size, on a machine that is also
running VMs. And it makes the single savings figure VDO reports mean two
different things at once, which would make `ember storage usage` lie
about what "compression" measures. With the default, the reported ratio
is compression and nothing else.

`--vdo-dedup` is worth having anyway: copy-on-write sharing between a VM
and its fork decays as both sides rewrite blocks, and deduplication
recovers some of that. When it is on, the reported ratio covers both
effects, and the spec for `usage` below says so.

Measured on a 60 GiB pool holding a 6.7 GiB Ubuntu dev image, one VM and
one fork of it, deduplication earned almost nothing: 2.64 GiB of data
without it, 2.58 GiB with, for 256 MiB more RAM. That is the expected
result rather than a disappointing one. dm-thin already shares
everything between a VM and its fork through copy-on-write, so there was
no duplication left below the pool to find. The case for turning it on
is a pool whose sharing has decayed over months of both sides rewriting
the same files, or two images pulled independently that happen to share
a base layer, and neither is visible in a workload built in one pass.

## Configuration

`GlobalConfig` gains one optional field:

```rust
/// dm-vdo layer beneath the dm-thin pool's data device. `None` for
/// pools without compression and for every non-dm-thin backend.
#[serde(default)]
pub vdo: Option<VdoConfig>,

pub struct VdoConfig {
    /// Physical bytes VDO manages on the backing device.
    pub physical_size: u64,
    /// Logical bytes VDO presents upward, which is the thin pool's
    /// data capacity. May exceed `physical_size`.
    pub logical_size: u64,
    /// Whether the deduplication index is consulted.
    pub deduplication: bool,
}
```

Both sizes are persisted rather than derived. The kernel requires the
table to state the volume's current logical and physical size exactly,
and a raw device that the operator grew externally would otherwise make
a derived physical size disagree with what the volume was formatted
with, failing activation with a bare `EINVAL`.

`vdo: None` on an existing dm-thin config means the pool has no VDO
layer, which is what every config written before this feature says. The
layer cannot be added to or removed from an existing pool: doing so
would mean rewriting every block. `ember init` refuses to change it, the
same way it already refuses to switch storage backends.

### CLI surface

```text
ember init --storage dm-thin --vdo [--vdo-logical-size <SIZE>] [--vdo-dedup]
ember storage grow [--size <SIZE>] [--logical-size <SIZE>]
```

`StorageBackend::grow` changes shape to carry the second dimension:
`grow(&self, GrowRequest) -> Result<()>`, where `GrowRequest` holds an
optional physical and an optional addressable size. ZFS and APFS keep
returning the errors they already did, so neither silently accepts a
size it would ignore.

The backend writes its own resolved sizes back to `config.json`. It is
the only layer that knows both the values and the instant they become
true, and returning them upward would put a dm-vdo detail in a trait
ZFS and APFS also implement.

`--vdo`, `--vdo-logical-size`, and `--vdo-dedup` are rejected unless
`--storage dm-thin` is selected, rather than being silently ignored.

## Lifecycle

### init

1. Verify the `vdo` device-mapper target is available (`modprobe
   dm-vdo`, then confirm it appears in `dmsetup targets`).
2. Verify `vdoformat` is on `PATH`, with an error naming the package
   when it is not.
3. Check the physical size against `vdo::MIN_PHYSICAL_BYTES`.
4. Create the sparse `data.img` (file mode) or accept the raw device.
5. Attach the loop device.
6. `vdoformat --uds-memory-size=<derived> --slab-bits=19
   --logical-size=<logical>K <dev>`.
   No `--force`: an already-formatted device means somebody else's data,
   and refusing is correct.
7. `dmsetup create ember-{ns}-vdo` with the table below.
8. Assemble the thin pool with the VDO device as its data device.

Failure after step 5 unwinds in reverse, tearing down the VDO device and
detaching loops, the same discipline the existing init already applies.

### Table line

```text
0 <logical_sectors> vdo V4 <backing_dev> <physical_4k_blocks> 4096 32768 16380 \
  compression on deduplication <on|off> maxDiscard <pool_block_4k>
```

Fixed choices, and why:

* **minimum I/O size 4096** is the kernel's recommended value.
* **every size is rounded down to a whole 4 KiB block** before it
  reaches the table or `vdoformat`. The tool records the volume's
  geometry in blocks and truncates, so a size that is not a whole block
  leaves the table claiming a sector the volume does not have, and the
  kernel answers with a bare `EINVAL` at activation.
* **block map cache 32768 blocks (128 MiB)** is both the minimum and the
  recommended value. The kernel documentation suggests scaling it with
  the working set, which we deliberately do not do: it costs about 1.15
  MB of RAM per MB of cache on a host that is also running VMs, and
  guessing a working set is worse than taking the documented default.
  This is the first knob to reach for if write throughput disappoints.
* **era length 16380** is the maximum and recommended value.
* **thread counts are left at kernel defaults.** Compression runs on the
  `cpu` threads, which default to 1, so this is the second knob. We take
  the default rather than invent a heuristic we have not measured.
* **maxDiscard** is set to the pool block size in 4 KiB units so a
  single dm-thin block discard passes down as one bio instead of being
  split into sixteen. It defaults to 1 (4 KiB) in the kernel, which
  would make reclaim needlessly slow.

### Activation

`ensure_pool_active` gains a step. Before assembling the pool it brings
up the VDO device if one is configured and not already present, and the
pool's data device becomes `/dev/mapper/ember-{ns}-vdo` instead of the
loop device.

If VDO comes up in read-only mode the backend refuses to proceed and
points at `vdoforcerebuild`, which is the only way out and is
deliberately a manual step because it can lose data. Recovery mode is
not an error: VDO repairs itself online.

### deinit

Teardown gains a step between removing the pool and detaching the loop
devices: remove the VDO device. Ordering matters, since the loop device
is still in use until VDO releases it.

### grow

`ember storage grow` grows three things that must stay consistent: the
backing file, VDO's physical size, and (when asked) VDO's logical size
plus the thin-pool table.

`--size` sets the new physical size and **preserves the logical to
physical ratio**. A pool created 300G/300G grown to 600G becomes
600G/600G. A pool deliberately over-provisioned 300G physical to 600G
logical grown to 600G physical becomes 600G/1200G. Doubling the disk
doubles the pool, whatever the operator's chosen bet was.

`--logical-size` overrides that and sets logical directly, which is how
an over-provision ratio gets changed after the fact, and how a pool that
was under-provisioned at level 2 gets fixed without buying disk.

Neither size may decrease: VDO forbids it and so does dm-thin. Physical
growth must also clear two kernel rules, 32832 4 KiB blocks (about 128
MiB) and one whole slab. At the pinned 2 GiB slab the second is always
the binding one, and both are checked before anything is resized rather
than surfacing as an opaque `dmsetup` failure.

`--size` is optional. Omitting it on a raw block device picks up the
device's current size, which is how an operator who grew an LV or a
cloud volume externally tells ember about it. ember never resizes a
block device it does not own.

The whole request is resolved and validated before anything is touched,
so a rejected grow cannot leave the backing file enlarged with nothing
to show for it. The baseline for "current size" is what the pool was
told it has, not the device underneath: those differ on a raw device
deliberately larger than the pool, and taking the device would read a
doubling as a shrink and let `--logical-size` swallow the rest of the
disk. A grow that would change nothing is refused rather than suspending
the live stack to load an identical table.

Sequence: truncate the backing file, refresh the loop device, reload the
VDO table with the new sizes, record them in `config.json`, then reload
the thin-pool table against the new logical size. The config write sits
between the two reloads deliberately. The kernel durably records VDO's
new sizes on a successful resume and demands them at every later
startup, so from that moment a config still describing the old ones is a
pool that cannot be activated. Persisting after the pool reload instead
would strand the installation whenever that reload was rejected.

The VDO table is swapped by loading into the inactive slot and resuming,
not by suspending first. That is what the target documents, and
suspending a volume with a live thin pool stacked on it risks blocking
on I/O the pool is still issuing.

## The ENOSPC cascade

This is the failure mode VDO introduces, and it deserves naming.

When VDO exhausts physical space it fails writes to the thin pool. The
pool sees I/O errors from its data device and drops into read-only or
failed mode. Every VM writing at that moment sees errors. The pool's own
free-space accounting gives no warning, because as far as it knows it
still has free data blocks: it is counting level 2 while the space ran
out at level 3.

This can happen even at the default 1:1 sizing, because VDO's metadata
reserve means usable physical is smaller than nominal physical, and
because incompressible data compresses to slightly more than its own
size once metadata is counted.

Three mitigations, and one honest admission.

1. `assert_pool_healthy`, which already gates allocating operations on
   thin-pool health, also checks VDO. Physical usage at or above
   `REFUSE_FULL_FRACTION` (95%) refuses image pulls, clones, forks, and
   resizes, since those are the operations that commit to consuming a
   lot more space.
2. Usage at or above `WARN_FULL_FRACTION` (85%) prints a warning to
   stderr but proceeds, as do a volume rebuilding its metadata and one
   whose compression has been switched off underneath us.
3. A read-only volume is refused everywhere, not only on allocating
   paths. `assert_read_write` runs on every activation, because
   `disk_device_path` is what `vm start` uses and a guest booted onto a
   read-only VDO just gets EIO with no explanation.
4. `ember storage usage` and `ember info` report VDO physical usage, so
   the number is visible without knowing to ask for it.

The admission: none of this stops a running VM from filling the pool
between commands. Command-time gating is what a CLI with no daemon can
do. An operator over-provisioning aggressively needs to watch the
number, and the docs say so rather than implying the tool has it covered.

## Usage reporting

`PoolUsage` gains one field:

```rust
/// Addressable space the pool exposes, when a compressing layer lets
/// it exceed `capacity`. `None` when the pool can only hand out the
/// physical space it actually has.
pub addressable: Option<u64>,
```

With VDO the dm-thin backend fills `PoolUsage` as:

| Field | Source |
| --- | --- |
| `capacity` | VDO total physical blocks |
| `allocated` | VDO data blocks plus overhead blocks |
| `reserved` | VDO overhead blocks |
| `logical` | thin-pool used data blocks, in bytes |
| `addressable` | thin-pool total data blocks, in bytes |
| `metadata` | thin-pool metadata device, unchanged |

VDO's own metadata lands in `reserved`, which is what that field is
for: space charged to the pool that holds no data and so must stay out
of the ratio. It is read from the `stats` message rather than from
`dmsetup status`, which reports data and metadata blocks pre-summed.

That distinction is not cosmetic. The reserve is several gigabytes on
any volume worth creating and barely moves with the amount of data
stored, so folding it into `allocated` alone divides the ratio by a wide
constant. Measured on a 60 GiB pool holding a 6.7 GiB Ubuntu image: 2.64
GiB of data plus 4.05 GiB of metadata. Charged together that reports
1.00x; charged apart it reports 2.5x, which is what actually happened.

One consequence is worth stating, because it looks like a discrepancy.
The reserve is *accounted* whether or not it has been written, so on a
sparse backing file a pool can report gigabytes used while its
`data.img` occupies far fewer blocks. In that same measurement the pool
reported 6.7 GiB used against 2.8 GiB actually allocated on the host
filesystem. Both are right, and which one matters depends on the
question. The pool's figure is what to plan capacity against, since VDO
will not let the pool into that space. The host filesystem's figure is
what a shared filesystem sees, and `du` is the tool for it.

`addressable` is read from the pool rather than from the recorded VDO
logical size. They are the same number up to the pool block the pool
rounds down to, and taking it from the pool reports what the kernel
actually has rather than what the config claims.

The existing accessors then mean the right things without special
casing. `free()` is real physical headroom. `compression_ratio()` is
thin-pool bytes stored divided by real bytes consumed. Headroom at level
2 is `addressable - logical`, since the bytes the pool has handed out
and the uncompressed bytes stored are the same number.

`addressable` is deliberately not called `vdo_logical_size`. It is a
property of a pool that can hand out more than it has, not a dm-vdo
detail, and `PoolUsage` is a shared core type that should not learn
subsystem vocabulary.

### Per-volume figures are pre-compression

VDO sits below the pool and has no idea which thin volume a physical
block belongs to. Compression therefore cannot be attributed per volume,
and `thin_ls` reports pre-compression bytes.

So with VDO active, `PROVISIONED`, `REFERENCED`, `EXCLUSIVE`, and
`SHARED` are all uncompressed, per-volume `COMPRESSION` stays `-`, and
only the pool-wide ratio is real. The CLI prints a footnote saying so,
derived from the data rather than from a flag: the pool reports a
logical size but no volume does, which is exactly the shape of "there is
a compressing layer the volumes cannot see into".

## Discard

Discard is what returns freed blocks to VDO, and the kernel documents it
as essential for a thinly provisioned VDO. The chain already works:
dm-thin enables discard passdown by default, VDO accepts discards, and
its 4 KiB granularity is below the pool block size, so the pool does not
disable passdown.

Deleting a VM or an image therefore frees real disk. What still does not
work is reclaiming space a guest freed inside its filesystem, because
Firecracker advertises no `VIRTIO_BLK_F_DISCARD` and the guest never
sends a discard at all. That is unchanged by this spec and is not
something it can fix.

## Module boundaries

VDO is not part of dm-thin. It is a device-mapper layer that this
backend happens to compose underneath its data device, and it owns its
own device name, table format, status parsing, and sizing rules.

That requires a small extraction first. The generic `dmsetup` plumbing
(`device_exists`, `create`, `remove`, `suspend`, `resume`, `reload`,
`swap_table`, `message`, `rename`, `status`, `list_with_prefix`,
`device_path`, and target loading) currently lives in `dm_thin` and
`dm_thin::pool`. It moves to a new `crate::dm` module, and both
`dm_thin` and `vdo` build on it. Without that, `vdo` would have to reach
into `dm_thin::pool` for `suspend`, which is precisely the cross-boundary
reach the project's conventions call out.

After the move:

* `crate::dm` — device-mapper primitives, no knowledge of any target.
* `crate::dm_thin` — thin pool and thin volumes: thin ids, the
  `thin-pool` table, `PoolStatus`, `MetadataSnap`, name derivation.
* `crate::vdo` — the `vdo` target: name derivation, the V4 table,
  `VdoStatus`, `vdoformat`, sizing.
* `crate::dm_thin_storage` — composes them into a `StorageBackend`.

## Hazards

**Logical size mismatch.** The table must state the volume's formatted
logical size exactly. A hand-edited `config.json` produces `EINVAL` from
`dmsetup create`, so the backend catches that specific failure and
explains what disagrees instead of passing the kernel's message through.

**Format refuses on a used device.** Deliberate. `vdoformat` without
`--force` will not overwrite an existing VDO volume, and ember does not
pass `--force`. A leftover `data.img` from a `deinit` without `--purge`
therefore blocks a fresh init, which is the right side to fail on. An
init that fails partway is the exception: it deletes a `data.img` that
same run created, since leaving a formatted file behind would block
every retry with no hint as to why.

**Re-init is refused outright.** `ember init` against an existing
installation bails before reaching storage. It has to: `init` zeroes the
thin metadata superblock, so merely getting as far as the backend
destroys every VM and image. The `config.json` write that used to be the
only thing in the way happens far too late.

**`--vdo` uses the whole backing device.** `vdoformat` takes no physical
size, so on a raw block device `--size` must match the device. A smaller
value would record a size the volume was never formatted with, and no
activation would ever succeed, so init rejects the mismatch instead.

**Two layers of "read-only".** Both the thin pool and VDO can go
read-only independently, with different recovery tools (`thin_repair`
versus `vdoforcerebuild`). Health errors name which layer is sick.

**Growth granularity.** VDO physical must grow by at least about 128 MiB
and at least one slab, and never above the size that yields 8192 slabs.
The first two are checked up front. The last is left to the kernel and
is not reachable in practice: 8192 slabs at 2 GiB is 16 TiB.

**Metadata sizing follows the addressable size.** The thin metadata
device is sized from what the pool can address, not from the disk under
it. Sizing it from the physical figure would leave an over-provisioned
pool exhausting metadata at the fraction of its capacity the two sizes
differ by, dropping it to read-only. That is the failure
over-provisioning is a bet against, and it would happen even when
compression delivered.

**RAM.** A VDO target costs roughly 38 MB fixed, about 150 MB for the
block map cache, 1.6 MB per TB of logical space, 268 MB per TB of
physical space, and the index memory when deduplication is on. On a laptop running VMs that is not free, and
`ember init --vdo` prints the estimate rather than letting the operator
discover it under memory pressure.

## Testing

Unit tests, no privileges, covering the pure functions:

* table line construction, including the optional-parameter tail
* `dmsetup status` parsing for every operating mode, index state, and a
  malformed line
* index memory derivation across the size range, including the rounding
  to accepted values and the floor
* logical-to-physical ratio preservation in `grow`, including the
  non-integer ratios and the u128 path that avoids overflow
* growth-granularity rejection
* `PoolUsage` mapping and the accessors derived from it
* the footnote predicate on `StorageUsage`

Integration tests, `#[ignore]`d, requiring root plus the `dm-vdo` module
and `vdoformat`. Run with `./run-integration-tests.sh vdo`:

* init with `--vdo` produces a formatted volume, an active VDO device,
  and a pool whose data device is that VDO device
* writing compressible data moves VDO's data blocks by less than the
  logical bytes the pool handed out, and the reported ratio clears 1.0.
  Measured against data blocks rather than total physical usage, since
  the metadata reserve is already several gigabytes before any data
  exists
* `deinit --purge` removes the VDO device and detaches the loops
* `storage grow` preserves the ratio and reloads both tables
* init refuses below `vdo::MIN_PHYSICAL_BYTES`, refuses `--vdo` on ZFS,
  and refuses to re-init a live installation without having zeroed the
  thin metadata superblock on the way
* a growth smaller than one slab is refused without having enlarged the
  backing file
* an over-provisioned pool's metadata device is sized for its
  addressable capacity, not its physical size

## External dependencies

* Linux 6.9 or newer for the in-tree `dm-vdo` target.
* `vdoformat`, from the `vdo` userspace package. Only for creating a
  volume; nothing at runtime needs it. Debian and Ubuntu ship
  it as `vdo`, Fedora and RHEL as `vdo`, Arch has it in the AUR as
  `vdo`. Only `vdoformat` is required; `vdostats` is not used, since
  `dmsetup status` reports the physical usage we need.
* `vdoforcerebuild`, from the same package, for manual recovery only.
