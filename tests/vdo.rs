//! Integration tests for the dm-vdo compression layer under dm-thin.
//!
//! These exercise the real CLI against real device-mapper state. They
//! are gated `#[ignore]` and Linux-only because they need:
//!
//! * root, for `dmsetup`, `losetup`, and `vdoformat`
//! * the `dm-vdo` kernel target (in-tree since Linux 6.9)
//! * `vdoformat`, from the `vdo` userspace package
//! * `dmsetup` (lvm2) and `thin-provisioning-tools`
//!
//! Run them with the project runner, which builds as the current user
//! and only runs the test binary under sudo:
//!
//! ```text
//! ./run-integration-tests.sh vdo
//! ```
//!
//! Unlike the dm-thin tests these do not use `/tmp`. A VDO volume's
//! minimum size is several gigabytes and `vdoformat` writes real
//! metadata into it, which on a tmpfs `/tmp` would land in RAM.

#![cfg(target_os = "linux")]

#[allow(dead_code)]
mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Smallest physical size `ember init --vdo` accepts. Mirrors
/// `ember_linux::vdo::MIN_PHYSICAL_BYTES`; the tests sit right on it so
/// `vdoformat` has as little to write as possible. The volume is sparse
/// either way, so the cost is VDO's own metadata, not the nominal size.
const MIN_PHYSICAL: &str = "32G";

// ---------------------------------------------------------------------------
// Preconditions and scratch space
// ---------------------------------------------------------------------------

fn require_vdo() {
    let _ = Command::new("modprobe").arg("dm-vdo").output();
    let targets = Command::new("dmsetup")
        .arg("targets")
        .output()
        .expect("failed to run dmsetup");
    let listing = String::from_utf8_lossy(&targets.stdout);
    assert!(
        listing
            .lines()
            .any(|l| l.split_whitespace().next() == Some("vdo")),
        "kernel does not provide the 'vdo' device-mapper target"
    );
    let formatter = Command::new("vdoformat").arg("--version").output();
    assert!(
        formatter.is_ok_and(|o| o.status.success()),
        "vdoformat is not installed (package 'vdo')"
    );
}

/// A scratch directory on real disk.
///
/// `tempfile::tempdir` follows `TMPDIR`, which is commonly a tmpfs. A
/// VDO volume is gigabytes before it holds anything, so these tests
/// default to the build directory instead and let `EMBER_VDO_TEST_DIR`
/// override it.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let base = std::env::var_os("EMBER_VDO_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let mut p = common::ember_bin();
                p.pop();
                p.join("vdo-tests")
            });
        let path = base.join(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("failed to create scratch directory");
        Self { path }
    }

    fn join(&self, s: &str) -> PathBuf {
        self.path.join(s)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `ember init --storage dm-thin --vdo` with a pinned instance id, so
/// the device names the assertions use are predictable.
fn init_vdo(
    state_dir: &Path,
    storage_path: &Path,
    id: &str,
    extra: &[&str],
) -> std::process::Output {
    let mut args = vec![
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--storage",
        "dm-thin",
        "--storage-path",
        storage_path.to_str().unwrap(),
        "--size",
        MIN_PHYSICAL,
        "--instance-id",
        id,
        "--vdo",
    ];
    args.extend_from_slice(extra);
    common::ember(&args)
}

fn assert_ok(output: &std::process::Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// `major:minor` of a device-mapper device, as `dmsetup deps` prints
/// dependencies. Used to prove the pool is stacked on VDO rather than
/// straight onto the loop device.
fn dm_major_minor(name: &str) -> String {
    let out = Command::new("dmsetup")
        .args(["info", "-c", "--noheadings", "-o", "major,minor", name])
        .output()
        .expect("failed to run dmsetup info");
    assert!(out.status.success(), "no such dm device: {name}");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .replace(',', ":")
}

fn dm_deps(name: &str) -> String {
    let out = Command::new("dmsetup")
        .args(["deps", name])
        .output()
        .expect("failed to run dmsetup deps");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn usage_json(state_dir: &Path) -> serde_json::Value {
    let out = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "storage",
        "usage",
        "--format",
        "json",
    ]);
    assert_ok(&out, "storage usage");
    serde_json::from_slice(&out.stdout).expect("storage usage emitted invalid JSON")
}

/// Push compressible bytes into the pool through a throwaway thin
/// volume, so the pool-level figures have something to report.
///
/// The payload is a repeating text pattern rather than zeroes: VDO
/// elides all-zero blocks entirely, which would prove nothing about
/// compression. Deduplication is off by default, so the repetition
/// across blocks is not what earns the ratio either.
fn write_compressible(pool: &str, instance_id: &str, mib: u64) {
    let thin_id = 4242;
    // Named inside the install's VM prefix on purpose: `deinit` sweeps
    // that prefix, so a failure to remove the device here cannot strand
    // the pool, the VDO device, and both loop devices while the scratch
    // directory is deleted out from under them.
    let dm_name = format!("ember-{instance_id}-vm-vdotestpayload");
    let dm_name = dm_name.as_str();
    let sectors = mib * 1024 * 1024 / 512;

    let msg = Command::new("dmsetup")
        .args(["message", pool, "0", &format!("create_thin {thin_id}")])
        .output()
        .expect("failed to run dmsetup message");
    assert!(msg.status.success(), "create_thin failed");

    let table = format!("0 {sectors} thin /dev/mapper/{pool} {thin_id}");
    let create = Command::new("dmsetup")
        .args(["create", dm_name, "--table", &table])
        .output()
        .expect("failed to run dmsetup create");
    assert!(
        create.status.success(),
        "activating the payload thin failed"
    );

    let payload: Vec<u8> = b"ember vdo compression test payload, repeated. "
        .iter()
        .copied()
        .cycle()
        .take(1024 * 1024)
        .collect();
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("/dev/mapper/{dm_name}"))
            .expect("failed to open the payload thin");
        for _ in 0..mib {
            f.write_all(&payload).expect("failed to write payload");
        }
        f.sync_all().expect("failed to flush payload");
    }

    let _ = Command::new("dmsetup").args(["remove", dm_name]).output();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Init builds the whole stack, the pool really is stacked on VDO, and
/// `deinit --purge` takes all of it back down.
#[test]
#[ignore = "requires root + dm-vdo + vdoformat"]
fn vdo_init_stacks_the_pool_on_vdo_and_deinit_removes_it() {
    require_vdo();
    let scratch = Scratch::new("round-trip");
    let state_dir = scratch.join("state");
    let storage_path = scratch.join("dm-thin");

    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };

    assert_ok(&init_vdo(&state_dir, &storage_path, "beef", &[]), "init");

    assert!(Path::new("/dev/mapper/ember-beef-vdo").exists());
    assert!(Path::new("/dev/mapper/ember-beef-pool").exists());

    // The pool's data device must be the VDO device, not the loop
    // device: that is the whole point of the layer, and everything
    // still "works" if it is wired up straight to the loop.
    let deps = dm_deps("ember-beef-pool");
    let vdo_dev = dm_major_minor("ember-beef-vdo");
    assert!(
        deps.contains(&format!("({})", vdo_dev.replace(':', ", "))) || deps.contains(&vdo_dev),
        "pool does not depend on the VDO device.\ndeps: {deps}\nvdo: {vdo_dev}"
    );

    let out = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "deinit",
        "--purge",
    ]);
    assert_ok(&out, "deinit");
    assert!(!Path::new("/dev/mapper/ember-beef-vdo").exists());
    assert!(!Path::new("/dev/mapper/ember-beef-pool").exists());
    assert!(!storage_path.join("data.img").exists());
}

/// The reported ratio comes out above 1.0 once compressible data is in
/// the pool, and the physical bytes VDO reports are genuinely fewer
/// than the logical bytes the pool handed out.
#[test]
#[ignore = "requires root + dm-vdo + vdoformat"]
fn vdo_reports_compression_once_the_pool_holds_data() {
    require_vdo();
    let scratch = Scratch::new("compression");
    let state_dir = scratch.join("state");
    let storage_path = scratch.join("dm-thin");

    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };
    assert_ok(&init_vdo(&state_dir, &storage_path, "cafe", &[]), "init");

    let before = usage_json(&state_dir);
    let before_used = before["pool"]["allocated"].as_u64().unwrap();

    write_compressible("ember-cafe-pool", "cafe", 512);

    let after = usage_json(&state_dir);
    let logical = after["pool"]["logical"].as_u64().expect("logical reported");
    let allocated = after["pool"]["allocated"].as_u64().unwrap();

    assert!(
        logical >= 512 * 1024 * 1024,
        "pool should have handed out at least what we wrote, got {logical}"
    );
    assert!(
        allocated > before_used,
        "physical usage did not move: {before_used} -> {allocated}"
    );
    // Against the delta, not the absolute. A VDO volume reserves
    // several gigabytes for its own metadata before storing anything,
    // and that reserve is charged to `allocated`, so on a pool this
    // size the absolute figure says nothing about compression.
    let physical_written = allocated - before_used;
    assert!(
        physical_written < logical,
        "512 MiB of compressible data took {physical_written} physical bytes for \
         {logical} logical, which is no compression at all"
    );
}

/// A 1:1 pool stays 1:1 as it grows, and the new sizes are written back
/// to `config.json`. Without the write-back the next activation asks
/// the kernel for the old sizes and is refused, so re-running any
/// command is the real assertion here.
#[test]
#[ignore = "requires root + dm-vdo + vdoformat"]
fn vdo_grow_preserves_the_ratio_and_persists_it() {
    require_vdo();
    let scratch = Scratch::new("grow");
    let state_dir = scratch.join("state");
    let storage_path = scratch.join("dm-thin");

    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };
    assert_ok(&init_vdo(&state_dir, &storage_path, "d00d", &[]), "init");

    let out = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "storage",
        "grow",
        "--size",
        "40G",
    ]);
    assert_ok(&out, "storage grow");

    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state_dir.join("config.json")).unwrap()).unwrap();
    let vdo = &config["vdo"];
    assert_eq!(vdo["physical_size"].as_u64().unwrap(), 40 << 30);
    assert_eq!(
        vdo["logical_size"].as_u64().unwrap(),
        40 << 30,
        "a 1:1 pool must stay 1:1"
    );

    // Tear the kernel state down and bring it back from config alone.
    // A stale recorded size fails here and nowhere earlier, because
    // `vdo::activate` asks the kernel for sizes the volume has already
    // durably recorded as different.
    for dev in ["ember-d00d-pool", "ember-d00d-vdo"] {
        let out = Command::new("dmsetup")
            .args(["remove", dev])
            .output()
            .expect("failed to run dmsetup remove");
        assert!(out.status.success(), "could not remove {dev}");
    }
    assert!(!Path::new("/dev/mapper/ember-d00d-vdo").exists());

    // `storage usage` deliberately does not activate, so it cannot be
    // the command that proves reactivation. A second grow can.
    let out = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "storage",
        "grow",
        "--size",
        "42G",
    ]);
    assert_ok(&out, "storage grow after a cold teardown");

    let usage = usage_json(&state_dir);
    assert_eq!(usage["pool"]["capacity"].as_u64().unwrap(), 42 << 30);
}

/// An over-provisioned pool exposes more than it holds, and says so.
#[test]
#[ignore = "requires root + dm-vdo + vdoformat"]
fn vdo_over_provisioned_pool_exposes_more_than_it_has() {
    require_vdo();
    let scratch = Scratch::new("over-provision");
    let state_dir = scratch.join("state");
    let storage_path = scratch.join("dm-thin");

    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };
    assert_ok(
        &init_vdo(
            &state_dir,
            &storage_path,
            "f00d",
            &["--vdo-logical-size", "128G"],
        ),
        "init",
    );

    let usage = usage_json(&state_dir);
    let capacity = usage["pool"]["capacity"].as_u64().unwrap();
    let addressable = usage["pool"]["addressable"].as_u64().unwrap();
    assert_eq!(
        capacity,
        32 << 30,
        "physical capacity is what --size asked for"
    );
    assert!(
        addressable > capacity,
        "addressable {addressable} should exceed physical {capacity}"
    );
    // `addressable` is the pool's own data capacity, so it is the VDO
    // logical size rounded down to a whole pool block.
    assert!(addressable >= (128 << 30) - (1 << 20));

    // Metadata has to be sized for what the pool can address, not for
    // the disk under it. This pool addresses four times its physical
    // size; sized from the physical figure the metadata device would
    // land under the 32 MiB floor, and the pool would exhaust metadata
    // at a quarter of its capacity and drop to read-only.
    const METADATA_FLOOR: u64 = 32 * 1024 * 1024;
    let metadata = usage["pool"]["metadata"]["capacity"].as_u64().unwrap();
    assert!(
        metadata > METADATA_FLOOR,
        "metadata {metadata} is no larger than the floor, so it was sized for the \
         physical size rather than the addressable one"
    );
}

/// Growth below one slab is refused, and refused before the backing
/// file has already been enlarged for it.
#[test]
#[ignore = "requires root + dm-vdo + vdoformat"]
fn vdo_grow_below_one_slab_is_refused_without_resizing_anything() {
    require_vdo();
    let scratch = Scratch::new("small-grow");
    let state_dir = scratch.join("state");
    let storage_path = scratch.join("dm-thin");

    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };
    assert_ok(&init_vdo(&state_dir, &storage_path, "5a1b", &[]), "init");

    let data_img = storage_path.join("data.img");
    let before = std::fs::metadata(&data_img).unwrap().len();

    // Clears the kernel's ~128 MiB floor but not the 2 GiB slab.
    let out = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "storage",
        "grow",
        "--size",
        "33600M",
    ]);
    assert!(!out.status.success(), "a sub-slab grow should be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("slab"), "{stderr}");

    let after = std::fs::metadata(&data_img).unwrap().len();
    assert_eq!(
        before, after,
        "a refused grow must not have enlarged the backing file"
    );
}

/// A pool too small to be worth compressing is refused up front, with
/// an explanation rather than a `vdoformat` failure.
#[test]
#[ignore = "requires root + dm-vdo + vdoformat"]
fn vdo_init_refuses_a_pool_below_the_minimum() {
    require_vdo();
    let scratch = Scratch::new("too-small");
    let state_dir = scratch.join("state");
    let storage_path = scratch.join("dm-thin");

    let out = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--storage",
        "dm-thin",
        "--storage-path",
        storage_path.to_str().unwrap(),
        "--size",
        "200M",
        "--vdo",
    ]);
    assert!(
        !out.status.success(),
        "init should have refused a 200M VDO pool"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("at least"),
        "error should explain the minimum: {stderr}"
    );
    // Nothing should have been built.
    assert!(!storage_path.join("data.img").exists());
}

/// Re-running init against a live installation is refused before it can
/// touch storage.
///
/// This is the dangerous one: `init` zeroes the thin metadata
/// superblock, so reaching the backend at all destroys every VM and
/// image in the pool. The guard has to fire in the CLI, not at the
/// `config.json` write that used to be the only thing standing in the
/// way.
#[test]
#[ignore = "requires root + dm-vdo + vdoformat"]
fn vdo_reinit_is_refused_before_storage_is_touched() {
    require_vdo();
    let scratch = Scratch::new("reinit");
    let state_dir = scratch.join("state");
    let storage_path = scratch.join("dm-thin");

    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };
    assert_ok(&init_vdo(&state_dir, &storage_path, "1234", &[]), "init");

    let metadata_before = std::fs::read(storage_path.join("metadata.img"))
        .map(|b| b[..4096].to_vec())
        .expect("metadata.img should be readable");

    // Identical arguments, and dropping --vdo, must both be refused.
    for extra in [vec!["--vdo"], vec![]] {
        let mut args = vec![
            "--state-dir",
            state_dir.to_str().unwrap(),
            "init",
            "--storage",
            "dm-thin",
            "--storage-path",
            storage_path.to_str().unwrap(),
            "--size",
            MIN_PHYSICAL,
            "--instance-id",
            "1234",
        ];
        args.extend_from_slice(&extra);
        let out = common::ember(&args);
        assert!(
            !out.status.success(),
            "re-init with {extra:?} should have been refused"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("already initialized"), "{stderr}");
    }

    // The pool is still there and its superblock was never zeroed.
    let metadata_after = std::fs::read(storage_path.join("metadata.img"))
        .map(|b| b[..4096].to_vec())
        .unwrap();
    assert_eq!(
        metadata_before, metadata_after,
        "re-init wrote to the thin metadata superblock"
    );
    assert!(Path::new("/dev/mapper/ember-1234-pool").exists());
}

/// `--vdo` on a backend with nowhere to put the layer is an error, not
/// a silently ignored flag.
#[test]
#[ignore = "requires root"]
fn vdo_is_refused_on_zfs() {
    let scratch = Scratch::new("wrong-backend");
    let state_dir = scratch.join("state");

    let out = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--storage",
        "zfs",
        "--vdo",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dm-thin"),
        "error should point at the right backend: {stderr}"
    );
}
