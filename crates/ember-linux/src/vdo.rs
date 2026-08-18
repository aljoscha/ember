//! The `vdo` device-mapper target: transparent compression and
//! optional deduplication.
//!
//! Ember composes a VDO volume underneath the dm-thin pool's data
//! device, so the pool stores compressed blocks without knowing it.
//! Nothing here knows about thin pools; this module owns the VDO device
//! name, the V4 table layout, status parsing, and the sizing rules, and
//! `dm_thin_storage` is what wires the two together.
//!
//! A VDO volume has two sizes that must be kept straight. Its
//! **physical** size is the real bytes it manages on the backing device.
//! Its **logical** size is what it presents upward, and may be larger
//! when the operator bets on compression. Both are recorded in the table
//! and must match what the volume was formatted with, so both are
//! persisted on `GlobalConfig` rather than derived.
//!
//! See `docs/VDO-SPEC.md` for the design.

use std::path::{Path, PathBuf};
use std::process::Command;

use ember_core::error::{Error, Result};

use crate::dm;

/// VDO addresses its backing store in 4 KiB blocks, independent of the
/// dm-thin pool's block size and of the 512-byte sector the rest of
/// device-mapper counts in.
pub const BLOCK_SIZE: u64 = 4096;

/// Minimum I/O size the volume accepts. 512 is legal; 4096 is the
/// kernel's recommendation and matches the block size.
const MIN_IO_SIZE: u64 = 4096;

/// Block map cache size in 4 KiB blocks (128 MiB), which the kernel
/// documents as both the minimum and the recommended value.
///
/// The documentation suggests scaling this with the working set, which
/// we deliberately do not do: it costs about 1.15 MB of RAM per MB of
/// cache on a host that is also running VMs, and guessing a working set
/// is worse than taking the documented default. This is the first knob
/// to reach for if write throughput disappoints.
const BLOCK_MAP_CACHE_BLOCKS: u64 = 32_768;

/// How eagerly the block map cache writes back dirty pages. 16380 is
/// the maximum and the recommended value; lower values trade steady
/// state writes for shorter rebuilds.
const BLOCK_MAP_ERA_LENGTH: u64 = 16_380;

/// Smallest physical size `ember init --vdo` accepts.
///
/// A VDO volume reserves at least 3 GB for metadata and its
/// deduplication index before it stores a single byte, and that reserve
/// is charged to physical usage. The floor is set where the reserve is
/// roughly a tenth of the volume, which matters for more than tidiness:
/// at the default 1:1 sizing the pool promises the full physical size,
/// so it only stays safe while compression clears
/// `size / (size - overhead)`. At 32 GiB that break-even is about
/// 1.1x, which any real filesystem beats. At 8 GiB it would be closer
/// to 1.6x, which is a bet rather than a default.
pub const MIN_PHYSICAL_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// Largest logical size the kernel accepts (4 PB).
const MAX_LOGICAL_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024 * 1024;

/// The kernel refuses a physical growth smaller than this many 4 KiB
/// blocks (about 128 MiB).
const MIN_GROWTH_BLOCKS: u64 = 32_832;

/// Slab size exponent handed to `vdoformat`, giving 2 GiB slabs.
///
/// This is `vdoformat`'s own default, pinned explicitly because we have
/// to know it: physical space can only grow one whole slab at a time,
/// and [`check_growth`] refuses a smaller increase rather than letting
/// `dmsetup` reject it with a bare `EINVAL`. Inheriting the tool's
/// default would leave that check guessing.
///
/// 2 GiB slabs cap the volume at the kernel's 8192 slabs times 2 GiB,
/// which is 16 TiB. Well past anything ember is used for, and larger
/// slabs would only trade that headroom for coarser growth.
const SLAB_BITS: u32 = 19;

/// Bytes in one slab, the granularity physical space grows by.
const SLAB_BYTES: u64 = (1 << SLAB_BITS) * BLOCK_SIZE;

/// Physical usage at or above this fraction refuses operations that
/// commit to consuming a lot more space.
///
/// Running out of physical space under a thin pool is not a graceful
/// failure: the pool sees I/O errors from its data device and drops
/// into read-only mode, taking every running VM with it. The pool's own
/// accounting gives no warning because it is counting logical space.
const REFUSE_FULL_FRACTION: f64 = 0.95;

/// Physical usage at or above this fraction warns but proceeds.
const WARN_FULL_FRACTION: f64 = 0.85;

/// Round a size down to a whole VDO block.
///
/// Every size that reaches the kernel is counted in 4 KiB blocks, and
/// `vdoformat` truncates the same way when it records the volume's
/// geometry. Normalizing here keeps the recorded size, the formatted
/// size, and the table in agreement. A size that is not a whole block
/// would leave the table claiming a sector the volume does not have,
/// and the kernel rejects that with a bare `EINVAL` at activation.
pub fn align_down(bytes: u64) -> u64 {
    bytes / BLOCK_SIZE * BLOCK_SIZE
}

/// VDO device name for an installation.
///
/// Mirrors the pool's derivation so the two names sort together in
/// `dmsetup ls`. There are no pre-namespace VDO volumes in the wild,
/// but `None` is handled for symmetry with configs that predate
/// per-installation isolation.
pub fn name(instance_id: Option<&str>) -> String {
    match instance_id {
        None => "ember-vdo".to_string(),
        Some(id) => format!("ember-{id}-vdo"),
    }
}

/// Path the VDO device is exposed at once activated.
pub fn device_path(vdo_name: &str) -> PathBuf {
    dm::device_path(vdo_name)
}

/// Deduplication index size, in quarter-gigabyte units.
///
/// `vdoformat --uds-memory-size` accepts 0.25, 0.5, or 0.75, or a whole
/// number of gigabytes. Quarters is the coarsest unit that represents
/// all of those exactly, which keeps [`arg`](Self::arg) free of float
/// formatting surprises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexMemory(u32);

impl IndexMemory {
    /// Largest size `vdoformat` accepts, in quarters.
    const MAX_QUARTERS: u32 = 1024 * 4;

    /// Index size covering `physical_bytes` of deduplication window.
    ///
    /// A dense index holds roughly one TB of window per GB of memory,
    /// so sizing by the pool's physical size means the window always
    /// covers the whole pool and there is no knob to get wrong. Rounded
    /// up to a value the tool accepts: quarters below one gigabyte, then
    /// whole gigabytes.
    pub fn for_physical_size(physical_bytes: u64) -> Self {
        const TIB: u128 = 1024 * 1024 * 1024 * 1024;
        let quarters = ((physical_bytes as u128) * 4).div_ceil(TIB).max(1);
        let quarters = if quarters <= 3 {
            quarters
        } else {
            quarters.div_ceil(4) * 4
        };
        Self(quarters.min(Self::MAX_QUARTERS as u128) as u32)
    }

    /// Render as the `--uds-memory-size` argument.
    pub fn arg(&self) -> String {
        match self.0 {
            1 => "0.25".to_string(),
            2 => "0.5".to_string(),
            3 => "0.75".to_string(),
            q => (q / 4).to_string(),
        }
    }

    /// Approximate RAM the index occupies while deduplication is on.
    pub fn ram_bytes(&self) -> u64 {
        (self.0 as u64) * (1024 * 1024 * 1024 / 4)
    }
}

/// Everything the table line needs beyond the backing device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// Bytes presented upward. Must match the formatted logical size.
    pub logical_size: u64,
    /// Bytes managed on the backing device. Must match the volume's
    /// current physical size.
    pub physical_size: u64,
    /// Whether the deduplication index is consulted. Compression is
    /// always on, since it is the reason this layer exists.
    pub deduplication: bool,
    /// Largest discard accepted, in 4 KiB blocks. Set from the dm-thin
    /// pool's block size so a single pool-block discard passes down as
    /// one bio; the kernel's own default of 1 would split it into
    /// sixteen.
    pub max_discard_blocks: u64,
}

/// Build the V4 table line.
///
/// Documented in `Documentation/admin-guide/device-mapper/vdo.rst`:
/// `<offset> <logical sectors> vdo V4 <storage dev> <storage 4k blocks>
/// <min io size> <block map cache blocks> <era length> [<key> <value> ...]`.
///
/// Thread counts are left off entirely so the kernel's defaults apply.
/// Compression runs on the `cpu` threads, which default to one, making
/// that the second knob after the block map cache.
fn table(backing: &Path, params: &Params) -> String {
    let logical_sectors = params.logical_size / dm::SECTOR_SIZE;
    let physical_blocks = params.physical_size / BLOCK_SIZE;
    let dedup = if params.deduplication { "on" } else { "off" };
    format!(
        "0 {logical_sectors} vdo V4 {} {physical_blocks} {MIN_IO_SIZE} \
         {BLOCK_MAP_CACHE_BLOCKS} {BLOCK_MAP_ERA_LENGTH} \
         compression on deduplication {dedup} maxDiscard {}",
        backing.display(),
        params.max_discard_blocks.max(1),
    )
}

/// Ensure the kernel provides the `vdo` target.
pub fn ensure_target_loaded() -> Result<()> {
    dm::ensure_target("dm-vdo", "vdo", "CONFIG_DM_VDO")
}

/// Ensure `vdoformat` is installed, naming the package when it is not.
///
/// The kernel target alone is not enough: a VDO volume has to be
/// formatted before it can be activated, and only the userspace tool
/// can do that.
pub fn ensure_format_tool() -> Result<()> {
    let found = Command::new("vdoformat")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if found {
        return Ok(());
    }
    Err(Error::Config(
        "vdoformat is not installed, and a VDO volume cannot be created without it. \
         It ships in the 'vdo' package on Debian, Ubuntu, Fedora, and RHEL, and in \
         the AUR as 'vdo' on Arch."
            .to_string(),
    ))
}

/// Largest physical size a volume can reach, from the kernel's cap of
/// 8192 slabs at our pinned slab size.
const MAX_PHYSICAL_BYTES: u64 = 8192 * SLAB_BYTES;

/// Reject a physical size the layer cannot sensibly manage.
pub fn check_physical_size(bytes: u64) -> Result<()> {
    if bytes < MIN_PHYSICAL_BYTES {
        return Err(Error::Config(format!(
            "--vdo needs at least {} of physical space (got {}). A VDO volume reserves \
             several gigabytes for its own metadata and deduplication index before it \
             stores anything, so below this the overhead outweighs the savings.",
            format_size(MIN_PHYSICAL_BYTES),
            format_size(bytes),
        )));
    }
    if bytes > MAX_PHYSICAL_BYTES {
        return Err(Error::Config(format!(
            "--vdo manages at most {} ({} slabs of {}), and {} was requested.",
            format_size(MAX_PHYSICAL_BYTES),
            8192,
            format_size(SLAB_BYTES),
            format_size(bytes),
        )));
    }
    Ok(())
}

/// Reject an addressable size the layer cannot sensibly expose.
///
/// The floor matters more than it looks: sizes are rounded down to a
/// whole block, so a small enough `--vdo-logical-size` rounds to zero,
/// and zero means "match the physical size" to `vdoformat` while
/// meaning a zero-length device in the table. The two then disagree and
/// activation fails with something that reads like a config mismatch.
pub fn check_logical_size(bytes: u64) -> Result<()> {
    if bytes < MIN_PHYSICAL_BYTES {
        return Err(Error::Config(format!(
            "--vdo-logical-size must be at least {} (got {}).",
            format_size(MIN_PHYSICAL_BYTES),
            format_size(bytes),
        )));
    }
    if bytes > MAX_LOGICAL_BYTES {
        return Err(Error::Config(format!(
            "--vdo-logical-size cannot exceed {} (got {}).",
            format_size(MAX_LOGICAL_BYTES),
            format_size(bytes),
        )));
    }
    Ok(())
}

/// Reject a growth the kernel would reject, with an explanation.
///
/// VDO cannot shrink in either dimension, and each physical growth must
/// add at least [`MIN_GROWTH_BLOCKS`] blocks. Catching that here beats
/// letting `dmsetup reload` fail with a bare `Invalid argument`.
pub fn check_growth(old: &Params, new: &Params) -> Result<()> {
    if new.physical_size < old.physical_size {
        return Err(Error::Config(format!(
            "a VDO volume cannot shrink: physical size is {} and {} was requested",
            format_size(old.physical_size),
            format_size(new.physical_size),
        )));
    }
    if new.logical_size < old.logical_size {
        return Err(Error::Config(format!(
            "a VDO volume cannot shrink: logical size is {} and {} was requested",
            format_size(old.logical_size),
            format_size(new.logical_size),
        )));
    }
    if new.logical_size > MAX_LOGICAL_BYTES {
        return Err(Error::Config(format!(
            "a VDO volume cannot address more than {} (got {})",
            format_size(MAX_LOGICAL_BYTES),
            format_size(new.logical_size),
        )));
    }
    let growth = new.physical_size - old.physical_size;
    if growth == 0 {
        return Ok(());
    }
    // Two separate kernel rules, and the slab one bites first at our
    // slab size. Checking both here beats letting `dmsetup` answer with
    // an undifferentiated `Invalid argument`.
    let minimum = (MIN_GROWTH_BLOCKS * BLOCK_SIZE).max(SLAB_BYTES);
    if growth < minimum {
        return Err(Error::Config(format!(
            "a VDO volume grows one {} slab at a time, so physical space must \
             increase by at least {} (got {})",
            format_size(SLAB_BYTES),
            format_size(minimum),
            format_size(growth),
        )));
    }
    Ok(())
}

/// Logical size that keeps the pool's over-provision ratio as physical
/// space grows.
///
/// A pool sized 1:1 stays 1:1, and one deliberately over-provisioned
/// 2:1 stays 2:1, so doubling the disk doubles the pool whatever bet the
/// operator made at init. The arithmetic goes through `u128` because
/// `old_logical * new_physical` overflows `u64` at a few terabytes.
/// Rounded down to a whole block, and never below the current logical
/// size, since VDO cannot shrink.
pub fn scale_logical(old_physical: u64, old_logical: u64, new_physical: u64) -> u64 {
    if old_physical == 0 {
        return old_logical;
    }
    let scaled = (old_logical as u128) * (new_physical as u128) / (old_physical as u128);
    let scaled = u64::try_from(scaled).unwrap_or(u64::MAX);
    (scaled / BLOCK_SIZE * BLOCK_SIZE).max(old_logical)
}

/// Format a fresh VDO volume on `backing`.
///
/// Deliberately does not pass `--force`: `vdoformat` refuses to
/// overwrite a device that already holds a VDO volume, and refusing is
/// the right side to fail on. Returns the tool's summary so init can
/// show the operator what it actually built.
pub fn format(backing: &Path, logical_size: u64, index: IndexMemory) -> Result<String> {
    // Expressed in KiB rather than bytes: `vdoformat` documents K as a
    // suffix but not B, and the size is a whole 4 KiB block by
    // construction, so KiB loses nothing.
    let logical_kib = align_down(logical_size) / 1024;
    let output = Command::new("vdoformat")
        .arg(format!("--uds-memory-size={}", index.arg()))
        .arg(format!("--slab-bits={SLAB_BITS}"))
        .arg(format!("--logical-size={logical_kib}K"))
        .arg(backing)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "vdoformat".to_string(),
            source: e,
        })?;
    let output = Error::check_command("vdoformat", output)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Activate a formatted VDO volume.
///
/// A size in the table that disagrees with what the volume was
/// formatted with is rejected as `EINVAL`, which on its own says
/// nothing useful, so that case gets an error naming the two sizes.
pub fn activate(vdo_name: &str, backing: &Path, params: &Params) -> Result<PathBuf> {
    dm::create(vdo_name, &table(backing, params))
        .map_err(|e| explain_einval(e, vdo_name, backing, params))?;
    Ok(device_path(vdo_name))
}

/// Replace the kernel's bare `EINVAL` with the reason it is almost
/// always caused by: sizes in the table that disagree with the sizes
/// the volume was formatted with.
fn explain_einval(e: Error, vdo_name: &str, backing: &Path, params: &Params) -> Error {
    if !dm::is_invalid_argument(&e) {
        return e;
    }
    Error::Pool(format!(
        "the kernel rejected the VDO table for '{vdo_name}'. The recorded sizes \
         (logical {}, physical {}) must match what the volume on {} was formatted with, \
         neither may shrink, and the backing device must be at least as large as the \
         physical size. Underlying error: {e}",
        format_size(params.logical_size),
        format_size(params.physical_size),
        backing.display(),
    ))
}

/// Swap in a table with new sizes on a live volume.
///
/// Loads into the inactive slot and resumes rather than suspending
/// first, which is what the target asks for and avoids blocking on I/O
/// the thin pool above is still issuing.
pub fn reload(vdo_name: &str, backing: &Path, params: &Params) -> Result<()> {
    dm::swap_table(vdo_name, &table(backing, params))
        .map_err(|e| explain_einval(e, vdo_name, backing, params))
}

/// Tear down the VDO device. The formatted volume on the backing device
/// survives for re-activation.
pub fn remove(vdo_name: &str) -> Result<()> {
    dm::remove(vdo_name)
}

/// Operating mode reported by `dmsetup status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VdoMode {
    /// Normal operation.
    Normal,
    /// Repairing its own metadata. Writes still work, so this is not an
    /// error, only slower.
    Recovering,
    /// An unrecoverable error forced the volume read-only. Getting out
    /// requires `vdoforcerebuild`, which can lose data and is therefore
    /// left to the operator.
    ReadOnly,
}

/// Status snapshot returned by [`status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VdoStatus {
    pub mode: VdoMode,
    /// Whether the kernel is actually compressing. Our table always
    /// asks for it, so this being false means something turned it off
    /// underneath us and the layer is doing nothing.
    pub compression_online: bool,
    /// Physical 4 KiB blocks in use, including VDO's own metadata.
    pub used_blocks: u64,
    pub total_blocks: u64,
}

impl VdoStatus {
    pub fn used_bytes(&self) -> u64 {
        self.used_blocks * BLOCK_SIZE
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_blocks * BLOCK_SIZE
    }

    /// Fraction of physical space consumed, 0.0 for an empty volume of
    /// unknown size rather than a division by zero.
    pub fn full_fraction(&self) -> f64 {
        if self.total_blocks == 0 {
            return 0.0;
        }
        self.used_blocks as f64 / self.total_blocks as f64
    }
}

/// Query VDO status via `dmsetup status`.
pub fn status(vdo_name: &str) -> Result<VdoStatus> {
    parse_status(&dm::status(vdo_name)?)
}

/// Parse a `dmsetup status` line for a vdo target.
///
/// `dmsetup` prefixes every status line with `<start> <length>
/// <target>`, and the vdo target then reports `<backing device>
/// <operating mode> <in recovery> <index state> <compression state>
/// <used blocks> <total blocks>`, for ten fields in all. The
/// `<in recovery>` field duplicates what `<operating mode>` already
/// says, and the index state is diagnostic only, so both are skipped.
///
/// `<used blocks>` counts VDO's own metadata alongside stored data, so
/// a freshly formatted volume is already several gigabytes in.
fn parse_status(line: &str) -> Result<VdoStatus> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 10 || fields[2] != "vdo" {
        return Err(Error::Command {
            command: "dmsetup status vdo".to_string(),
            exit_code: 0,
            stderr: format!("unexpected status format: {line}"),
        });
    }
    let mode = match fields[4] {
        "normal" => VdoMode::Normal,
        "recovering" => VdoMode::Recovering,
        "read-only" => VdoMode::ReadOnly,
        other => {
            return Err(Error::Command {
                command: "dmsetup status vdo".to_string(),
                exit_code: 0,
                stderr: format!("unknown vdo operating mode: {other}"),
            });
        }
    };
    Ok(VdoStatus {
        mode,
        compression_online: fields[7] == "online",
        used_blocks: parse_blocks(fields[8], "used physical blocks")?,
        total_blocks: parse_blocks(fields[9], "total physical blocks")?,
    })
}

fn parse_blocks(s: &str, field: &str) -> Result<u64> {
    s.parse::<u64>().map_err(|e| Error::Command {
        command: "dmsetup status vdo".to_string(),
        exit_code: 0,
        stderr: format!("non-numeric {field} value {s:?}: {e}"),
    })
}

/// Space accounting read from the volume's own statistics.
///
/// `dmsetup status` reports data and metadata blocks pre-summed, which
/// is not enough: VDO's metadata reserve is several gigabytes on any
/// volume worth creating, so a summed figure buries the compression it
/// is supposed to reveal. The `stats` message reports them separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VdoStats {
    /// Physical blocks holding user data, after compression. This is
    /// the denominator of any honest compression ratio.
    pub data_blocks: u64,
    /// Physical blocks charged to VDO's own metadata: block map,
    /// journals, slab structures, and the deduplication index.
    ///
    /// Near-constant for a given geometry, and reserved whether or not
    /// it has been written. On a sparse backing file most of it costs
    /// no real disk, which is why a pool can report gigabytes used
    /// while its `data.img` occupies far fewer blocks.
    pub overhead_blocks: u64,
    /// Total physical blocks the volume manages.
    pub physical_blocks: u64,
}

impl VdoStats {
    /// Physical bytes consumed, VDO's own metadata included.
    pub fn used_bytes(&self) -> u64 {
        (self.data_blocks + self.overhead_blocks) * BLOCK_SIZE
    }

    /// Physical bytes holding user data.
    pub fn data_bytes(&self) -> u64 {
        self.data_blocks * BLOCK_SIZE
    }

    /// Physical bytes charged to metadata rather than to data.
    pub fn overhead_bytes(&self) -> u64 {
        self.overhead_blocks * BLOCK_SIZE
    }

    pub fn total_bytes(&self) -> u64 {
        self.physical_blocks * BLOCK_SIZE
    }
}

/// Read per-volume space accounting via the `stats` message.
pub fn stats(vdo_name: &str) -> Result<VdoStats> {
    parse_stats(&dm::message_response(vdo_name, "stats")?)
}

/// Parse the `stats` message response.
///
/// The kernel emits a brace-delimited blob of `key : value` pairs
/// separated by commas, with nested groups for per-subsystem counters
/// (`message-stats.c`). We want three top-level fields and ignore the
/// rest, so this looks up keys rather than modelling the structure.
fn parse_stats(blob: &str) -> Result<VdoStats> {
    let field = |key: &str| -> Result<u64> {
        blob.split(',')
            .filter_map(|token| token.split_once(':'))
            .find(|(k, _)| {
                k.trim_matches(|c: char| c == '{' || c == '}' || c.is_whitespace()) == key
            })
            .ok_or_else(|| Error::Command {
                command: "dmsetup message vdo stats".to_string(),
                exit_code: 0,
                stderr: format!("no {key} field in the vdo stats response"),
            })
            .and_then(|(_, v)| {
                let v = v.trim_matches(|c: char| c == '{' || c == '}' || c.is_whitespace());
                v.parse::<u64>().map_err(|e| Error::Command {
                    command: "dmsetup message vdo stats".to_string(),
                    exit_code: 0,
                    stderr: format!("non-numeric {key} value {v:?}: {e}"),
                })
            })
    };
    Ok(VdoStats {
        data_blocks: field("dataBlocksUsed")?,
        overhead_blocks: field("overheadBlocksUsed")?,
        physical_blocks: field("physicalBlocks")?,
    })
}

/// Refuse to proceed when the volume is in no state to take writes.
///
/// Read-only is fatal and needs `vdoforcerebuild`. A volume close to
/// full is refused because the caller is about to commit to consuming a
/// lot more space, and running out under a thin pool takes the pool
/// read-only rather than failing one operation.
pub fn assert_healthy(vdo_name: &str, st: &VdoStatus) -> Result<()> {
    assert_read_write(vdo_name, st)?;
    if st.full_fraction() >= REFUSE_FULL_FRACTION {
        return Err(Error::Pool(format!(
            "VDO volume '{vdo_name}' is {:.1}% full ({} of {} physical). Allocating more \
             would risk exhausting it, which takes the whole thin pool read-only. \
             Run `ember storage grow --size <bigger>`, or delete a VM or image to \
             release blocks.",
            st.full_fraction() * 100.0,
            format_size(st.used_bytes()),
            format_size(st.total_bytes()),
        )));
    }
    Ok(())
}

/// Refuse a volume that cannot take writes at all.
///
/// Separate from [`assert_healthy`] because it is the one check that
/// belongs on every path that hands the device to something, not just
/// on the paths that allocate.
pub fn assert_read_write(vdo_name: &str, st: &VdoStatus) -> Result<()> {
    if st.mode != VdoMode::ReadOnly {
        return Ok(());
    }
    Err(Error::Pool(format!(
        "VDO volume '{vdo_name}' is read-only, which means it hit an unrecoverable \
         error. Stop everything using the pool and run `vdoforcerebuild` on the \
         backing device to let it rebuild its metadata. Some data may be lost."
    )))
}

/// Warnings worth printing before proceeding: conditions that are not
/// fatal but that nothing else would ever tell the operator about.
pub fn warnings(vdo_name: &str, st: &VdoStatus) -> Vec<String> {
    let mut out = Vec::new();
    let fraction = st.full_fraction();
    if (WARN_FULL_FRACTION..REFUSE_FULL_FRACTION).contains(&fraction) {
        out.push(format!(
            "warning: VDO volume '{vdo_name}' is {:.1}% full ({} of {} physical). \
             Running out takes the thin pool read-only; grow it before that happens.",
            fraction * 100.0,
            format_size(st.used_bytes()),
            format_size(st.total_bytes()),
        ));
    }
    if st.mode == VdoMode::Recovering {
        out.push(format!(
            "warning: VDO volume '{vdo_name}' is rebuilding its metadata. \
             Writes still work, but everything is slower until it finishes."
        ));
    }
    if !st.compression_online {
        out.push(format!(
            "warning: compression is switched off on VDO volume '{vdo_name}', so the \
             layer is costing space and CPU without saving anything. Turn it back on \
             with `dmsetup message {vdo_name} 0 compression on`."
        ));
    }
    out
}

/// Rough RAM the kernel needs for a volume of this shape.
///
/// From the per-target figures the kernel documents: a fixed cost, the
/// block map cache plus its overhead, and terms scaling with logical and
/// physical size. Printed at init so the operator sees the cost before
/// meeting it under memory pressure, not used for any decision.
pub fn ram_estimate_bytes(params: &Params, index: IndexMemory) -> u64 {
    const MIB: u64 = 1024 * 1024;
    const TIB: u64 = 1024 * 1024 * 1024 * 1024;
    let fixed = 38 * MIB;
    // 1.15 MB of RAM per MB of block map cache.
    let cache = BLOCK_MAP_CACHE_BLOCKS * BLOCK_SIZE * 115 / 100;
    let logical = (params.logical_size as u128 * (16 * MIB as u128 / 10) / TIB as u128) as u64;
    let physical = (params.physical_size as u128 * (268 * MIB as u128) / TIB as u128) as u64;
    let dedup = if params.deduplication {
        index.ram_bytes()
    } else {
        0
    };
    fixed + cache + logical + physical + dedup
}

/// Human-readable byte count that picks its own unit.
///
/// A fixed unit is not good enough here: the growth minimum is 128 MiB,
/// and rendering both it and a rejected 64 MiB request as "0.1 GiB"
/// made the error contradict itself.
fn format_size(bytes: u64) -> String {
    const TIB: u64 = 1024 * GIB;
    const GIB: u64 = 1024 * MIB;
    const MIB: u64 = 1024 * 1024;
    if bytes >= TIB {
        format!("{:.1} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    fn params(logical: u64, physical: u64) -> Params {
        Params {
            logical_size: logical,
            physical_size: physical,
            deduplication: false,
            max_discard_blocks: 16,
        }
    }

    #[test]
    fn table_matches_documented_shape() {
        let t = table(Path::new("/dev/loop3"), &params(2 * GIB, GIB));
        assert_eq!(
            t,
            "0 4194304 vdo V4 /dev/loop3 262144 4096 32768 16380 \
             compression on deduplication off maxDiscard 16"
        );
    }

    /// Compression is the reason the layer exists and the kernel
    /// defaults it off, so it must always be in the table.
    #[test]
    fn table_always_enables_compression() {
        let mut p = params(GIB, GIB);
        p.deduplication = true;
        let t = table(Path::new("/dev/loop0"), &p);
        assert!(t.contains("compression on"), "{t}");
        assert!(t.contains("deduplication on"), "{t}");
    }

    /// The kernel rejects maxDiscard 0. A pool block size below 4 KiB
    /// is not reachable today, but the floor keeps a future one from
    /// producing an unloadable table.
    #[test]
    fn table_floors_max_discard_at_one() {
        let mut p = params(GIB, GIB);
        p.max_discard_blocks = 0;
        assert!(table(Path::new("/dev/loop0"), &p).ends_with("maxDiscard 1"));
    }

    #[test]
    fn index_memory_rounds_up_to_accepted_values() {
        // Below a terabyte lands on the fractional settings.
        assert_eq!(IndexMemory::for_physical_size(32 * GIB).arg(), "0.25");
        assert_eq!(IndexMemory::for_physical_size(256 * GIB).arg(), "0.25");
        assert_eq!(IndexMemory::for_physical_size(257 * GIB).arg(), "0.5");
        assert_eq!(IndexMemory::for_physical_size(512 * GIB).arg(), "0.5");
        assert_eq!(IndexMemory::for_physical_size(600 * GIB).arg(), "0.75");
        // At and above a terabyte, whole gigabytes only.
        assert_eq!(IndexMemory::for_physical_size(TIB).arg(), "1");
        assert_eq!(IndexMemory::for_physical_size(TIB + 1).arg(), "2");
        assert_eq!(IndexMemory::for_physical_size(4 * TIB).arg(), "4");
    }

    /// An empty or absurd pool still has to produce something the tool
    /// accepts.
    #[test]
    fn index_memory_is_clamped_at_both_ends() {
        assert_eq!(IndexMemory::for_physical_size(0).arg(), "0.25");
        assert_eq!(IndexMemory::for_physical_size(u64::MAX).arg(), "1024");
    }

    #[test]
    fn index_memory_ram_matches_its_setting() {
        assert_eq!(
            IndexMemory::for_physical_size(32 * GIB).ram_bytes(),
            GIB / 4
        );
        assert_eq!(IndexMemory::for_physical_size(2 * TIB).ram_bytes(), 2 * GIB);
    }

    #[test]
    fn scale_logical_preserves_one_to_one() {
        assert_eq!(scale_logical(300 * GIB, 300 * GIB, 600 * GIB), 600 * GIB);
    }

    #[test]
    fn scale_logical_preserves_over_provisioning() {
        assert_eq!(scale_logical(300 * GIB, 600 * GIB, 600 * GIB), 1200 * GIB);
        // A ratio that does not divide evenly still scales, rounded
        // down to a whole block.
        let scaled = scale_logical(300 * GIB, 450 * GIB, 500 * GIB);
        assert_eq!(scaled, 750 * GIB);
        assert_eq!(scaled % BLOCK_SIZE, 0);
    }

    /// `old_logical * new_physical` overflows `u64` above a few
    /// terabytes, which is why the arithmetic goes through `u128`.
    #[test]
    fn scale_logical_survives_petabyte_inputs() {
        let pib = 1024 * TIB;
        assert_eq!(scale_logical(pib, 2 * pib, 2 * pib), 4 * pib);
    }

    #[test]
    fn scale_logical_never_shrinks() {
        // Physical staying put must not round logical down.
        assert_eq!(scale_logical(300 * GIB, 300 * GIB, 300 * GIB), 300 * GIB);
        // A degenerate recorded size cannot produce a shrink either.
        assert_eq!(scale_logical(0, 300 * GIB, 600 * GIB), 300 * GIB);
    }

    #[test]
    fn growth_must_not_shrink_either_dimension() {
        let old = params(300 * GIB, 300 * GIB);
        assert!(check_growth(&old, &params(300 * GIB, 200 * GIB)).is_err());
        assert!(check_growth(&old, &params(200 * GIB, 300 * GIB)).is_err());
    }

    /// Physical space grows a whole slab at a time. The kernel's own
    /// ~128 MiB floor is the looser of the two rules at our slab size,
    /// so the slab rule is what actually rejects things.
    #[test]
    fn growth_must_clear_a_whole_slab() {
        let old = params(300 * GIB, 300 * GIB);
        for too_small in [64 * 1024 * 1024, 300 * 1024 * 1024, SLAB_BYTES - 1] {
            let err = check_growth(&old, &params(300 * GIB, 300 * GIB + too_small))
                .unwrap_err()
                .to_string();
            assert!(err.contains("slab"), "{too_small}: {err}");
        }
        assert!(check_growth(&old, &params(300 * GIB, 300 * GIB + SLAB_BYTES)).is_ok());
        assert!(check_growth(&old, &params(300 * GIB, 600 * GIB)).is_ok());
    }

    #[test]
    fn growth_is_capped_at_the_kernel_logical_maximum() {
        let old = params(300 * GIB, 300 * GIB);
        let over = params(MAX_LOGICAL_BYTES + BLOCK_SIZE, 300 * GIB);
        assert!(check_growth(&old, &over).is_err());
        assert!(check_growth(&old, &params(MAX_LOGICAL_BYTES, 300 * GIB)).is_ok());
    }

    #[test]
    fn align_down_truncates_to_whole_blocks() {
        assert_eq!(align_down(0), 0);
        assert_eq!(align_down(BLOCK_SIZE), BLOCK_SIZE);
        assert_eq!(align_down(BLOCK_SIZE - 1), 0);
        assert_eq!(align_down(BLOCK_SIZE + 512), BLOCK_SIZE);
        // A K-suffixed size that is not a multiple of 4.
        assert_eq!(align_down(1025 * 1024), 1024 * 1024);
    }

    /// The error has to distinguish the minimum from the request, which
    /// a GiB-only formatter could not: both rendered as "0.1 GiB".
    #[test]
    fn size_formatting_scales_to_the_value() {
        assert_eq!(format_size(64 * 1024 * 1024), "64.0 MiB");
        assert_eq!(format_size(SLAB_BYTES), "2.0 GiB");
        assert_eq!(format_size(3 * 1024 * GIB), "3.0 TiB");
        assert_eq!(format_size(512), "512 B");
    }

    /// Growing only the logical side leaves physical untouched, which
    /// must not trip the minimum-growth check.
    #[test]
    fn logical_only_growth_is_allowed() {
        let old = params(300 * GIB, 300 * GIB);
        assert!(check_growth(&old, &params(600 * GIB, 300 * GIB)).is_ok());
    }

    #[test]
    fn physical_size_bounds_are_enforced() {
        assert!(check_physical_size(MIN_PHYSICAL_BYTES).is_ok());
        assert!(check_physical_size(MIN_PHYSICAL_BYTES - 1).is_err());
        assert!(check_physical_size(MAX_PHYSICAL_BYTES).is_ok());
        assert!(check_physical_size(MAX_PHYSICAL_BYTES + 1).is_err());
        // The cap is the kernel's 8192 slabs, which at 2 GiB is 16 TiB.
        assert_eq!(MAX_PHYSICAL_BYTES, 16 * TIB);
    }

    /// A logical size small enough to round to zero would mean
    /// "default to physical" to `vdoformat` and a zero-length device in
    /// the table, which disagree.
    #[test]
    fn logical_size_bounds_are_enforced() {
        assert!(check_logical_size(MIN_PHYSICAL_BYTES).is_ok());
        assert!(check_logical_size(1024).is_err());
        assert!(check_logical_size(align_down(1024)).is_err());
        assert!(check_logical_size(MAX_LOGICAL_BYTES).is_ok());
        assert!(check_logical_size(MAX_LOGICAL_BYTES + BLOCK_SIZE).is_err());
    }

    /// The floor exists so that the default 1:1 sizing is not secretly
    /// a bet on compression. Pin the reasoning: the metadata reserve
    /// must stay a small enough slice that break-even is easy.
    #[test]
    fn floor_keeps_the_one_to_one_default_safe() {
        const RESERVE: f64 = 3.0 * 1000.0 * 1000.0 * 1000.0;
        let physical = MIN_PHYSICAL_BYTES as f64;
        let break_even = physical / (physical - RESERVE);
        assert!(
            break_even < 1.2,
            "break-even ratio {break_even} is too high"
        );
    }

    /// Shaped like the real `stats` response: a brace-delimited blob of
    /// `key : value` pairs with nested groups for per-subsystem
    /// counters. Numbers are from a 60 GiB pool holding a 6.7 GiB
    /// Ubuntu image.
    const STATS_BLOB: &str = "{ version : 41, releaseVersion : 133524, \
        dataBlocksUsed : 692060, overheadBlocksUsed : 1061683, \
        logicalBlocksUsed : 1746109, physicalBlocks : 15728640, \
        logicalBlocks : 15728640, blockMapCacheSize : 134217728, \
        blockSize : 4096, completeRecoveries : 0, mode : normal, \
        allocator : { slabCount : 28, slabsOpened : 1, slabsReopened : 0 }, \
        journal : { started : 12, written : 12, committed : 12 } }";

    #[test]
    fn parses_the_stats_response() {
        let st = parse_stats(STATS_BLOB).unwrap();
        assert_eq!(st.data_blocks, 692_060);
        assert_eq!(st.overhead_blocks, 1_061_683);
        assert_eq!(st.physical_blocks, 15_728_640);
        assert_eq!(st.data_bytes(), 692_060 * 4096);
        assert_eq!(st.overhead_bytes(), 1_061_683 * 4096);
        assert_eq!(st.used_bytes(), (692_060 + 1_061_683) * 4096);
        assert_eq!(st.total_bytes(), 60 * 1024 * 1024 * 1024);
    }

    /// The nested groups reuse short key names, so a substring match
    /// would pick up the wrong number. Keys are compared whole.
    #[test]
    fn stats_keys_are_matched_whole_not_by_substring() {
        let st = parse_stats(STATS_BLOB).unwrap();
        // `logicalBlocks` sits next to `logicalBlocksUsed`, and
        // `physicalBlocks` must not be read from either.
        assert_eq!(st.physical_blocks, 15_728_640);
        // A blob whose only match is a longer key must not resolve.
        let decoy = "{ metadataBlocksUsed : 5, overheadBlocksUsed : 7, physicalBlocks : 9 }";
        assert!(parse_stats(decoy).is_err(), "dataBlocksUsed is absent");
    }

    /// A missing or unparseable field is a hard error. Silently
    /// reporting no overhead would divide the compression ratio by
    /// several gigabytes of metadata and understate it to about 1.00x.
    #[test]
    fn stats_rejects_a_response_it_cannot_trust() {
        assert!(parse_stats("").is_err());
        assert!(parse_stats("{ dataBlocksUsed : 1, physicalBlocks : 2 }").is_err());
        assert!(
            parse_stats("{ dataBlocksUsed : x, overheadBlocksUsed : 1, physicalBlocks : 2 }")
                .is_err()
        );
    }

    fn status_line(mode: &str, used: u64, total: u64) -> String {
        format!("0 4194304 vdo ember-a3f4-vdo {mode} - online online {used} {total}")
    }

    #[test]
    fn parses_a_normal_status_line() {
        let st = parse_status(&status_line("normal", 1000, 10_000)).unwrap();
        assert_eq!(st.mode, VdoMode::Normal);
        assert!(st.compression_online);
        assert_eq!(st.used_bytes(), 1000 * 4096);
        assert_eq!(st.total_bytes(), 10_000 * 4096);
        assert!((st.full_fraction() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn parses_every_operating_mode() {
        for (text, expected) in [
            ("normal", VdoMode::Normal),
            ("recovering", VdoMode::Recovering),
            ("read-only", VdoMode::ReadOnly),
        ] {
            let st = parse_status(&status_line(text, 0, 1)).unwrap();
            assert_eq!(st.mode, expected, "{text}");
        }
    }

    /// Deduplication off leaves the index offline, which is normal and
    /// must not be confused with compression being off, which is not.
    #[test]
    fn separates_an_offline_index_from_offline_compression() {
        let dedup_off = "0 4194304 vdo ember-a3f4-vdo normal - offline online 5 100";
        assert!(parse_status(dedup_off).unwrap().compression_online);
        assert!(warnings("v", &parse_status(dedup_off).unwrap()).is_empty());

        let compression_off = "0 4194304 vdo ember-a3f4-vdo normal - online offline 5 100";
        let st = parse_status(compression_off).unwrap();
        assert!(!st.compression_online);
        let w = warnings("v", &st);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("compression is switched off"), "{w:?}");
    }

    /// A line from a different target, a truncated line, or an
    /// operating mode we do not know must fail loudly rather than be
    /// read as zero usage.
    #[test]
    fn rejects_lines_it_cannot_trust() {
        assert!(parse_status("0 100 thin-pool 0 1/2 3/4 - rw").is_err());
        assert!(parse_status("0 4194304 vdo dev normal - online online 5").is_err());
        assert!(parse_status(&status_line("melted", 0, 1)).is_err());
        assert!(parse_status(
            &status_line("normal", 0, 1).replace("online online 0", "online online x")
        )
        .is_err());
    }

    #[test]
    fn empty_volume_reports_zero_fullness_not_a_division_by_zero() {
        let st = parse_status(&status_line("normal", 0, 0)).unwrap();
        assert_eq!(st.full_fraction(), 0.0);
        assert!(assert_healthy("v", &st).is_ok());
    }

    #[test]
    fn read_only_is_refused_whatever_the_usage() {
        let st = parse_status(&status_line("read-only", 0, 10_000)).unwrap();
        let err = assert_healthy("v", &st).unwrap_err().to_string();
        assert!(err.contains("vdoforcerebuild"), "{err}");
    }

    /// Recovering is slow, not fatal: VDO repairs itself online. It
    /// still gets said out loud, since nothing else would mention it.
    #[test]
    fn recovering_warns_but_does_not_refuse() {
        let st = parse_status(&status_line("recovering", 10, 10_000)).unwrap();
        assert!(assert_healthy("v", &st).is_ok());
        let w = warnings("v", &st);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("rebuilding"), "{w:?}");
    }

    #[test]
    fn fullness_bands_refuse_warn_and_pass() {
        // Refused, and not also warned about: the error already said it.
        let refused = parse_status(&status_line("normal", 9_500, 10_000)).unwrap();
        assert!(assert_healthy("v", &refused).is_err());
        assert!(warnings("v", &refused).is_empty());

        let warned = parse_status(&status_line("normal", 8_500, 10_000)).unwrap();
        assert!(assert_healthy("v", &warned).is_ok());
        assert_eq!(warnings("v", &warned).len(), 1);

        let fine = parse_status(&status_line("normal", 8_499, 10_000)).unwrap();
        assert!(assert_healthy("v", &fine).is_ok());
        assert!(warnings("v", &fine).is_empty());
    }

    #[test]
    fn names_embed_the_namespace() {
        assert_eq!(name(Some("a3f4")), "ember-a3f4-vdo");
        assert_eq!(name(None), "ember-vdo");
    }

    /// Not a precise figure, but it has to land in the right order of
    /// magnitude or printing it would mislead rather than inform.
    #[test]
    fn ram_estimate_is_plausible() {
        let ram = ram_estimate_bytes(&params(300 * GIB, 300 * GIB), IndexMemory(1));
        assert!(
            (200 * 1024 * 1024..400 * 1024 * 1024).contains(&ram),
            "{ram} bytes"
        );
        // Deduplication adds its index, physical size dominates at scale.
        let mut big = params(4 * TIB, 4 * TIB);
        big.deduplication = true;
        assert!(ram_estimate_bytes(&big, IndexMemory::for_physical_size(4 * TIB)) > 5 * GIB);
    }
}
