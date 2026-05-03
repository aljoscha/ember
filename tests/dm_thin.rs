//! Integration tests for the dm-thin storage backend.
//!
//! These tests exercise the real CLI binary against real device-mapper
//! state. They are gated `#[ignore]` and only run on Linux because:
//!
//! * dm-thin requires the `dm-thin-pool` kernel module + root
//!   privileges for `dmsetup`, `losetup`, and friends.
//! * The host must have `dmsetup` (lvm2), `thin-provisioning-tools`,
//!   and `e2fsprogs` available.
//!
//! Run them explicitly with:
//!
//! ```text
//! sudo cargo test --test dm_thin -- --ignored --test-threads=1
//! ```

#![cfg(target_os = "linux")]

// Each integration-test crate compiles `tests/common/` as its own
// top-level module; only `common::ember` is used here, so without this
// attribute clippy reports every other shared helper as dead code.
#[allow(dead_code)]
mod common;

use std::path::Path;

/// Run `ember init --storage dm-thin` against a tempdir, then verify
/// `ember deinit --purge` cleans up. Smoke test for the new init +
/// deinit paths added in Phase 5/7.
#[test]
#[ignore = "requires root + dm-thin kernel module"]
fn dm_thin_init_and_deinit_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let storage_path = tmp.path().join("dm-thin");
    let state_dir = tmp.path().join("state");

    // Always tear down on the way out, even if assertions below panic.
    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };

    // Init. Pin the instance id so the pool name we assert on
    // matches what `ember init` actually creates.
    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--storage",
        "dm-thin",
        "--storage-path",
        storage_path.to_str().unwrap(),
        "--size",
        "200M",
        "--instance-id",
        "dead",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "init failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        Path::new("/dev/mapper/ember-dead-pool").exists(),
        "ember-dead-pool should be active after init"
    );
    assert!(storage_path.join("metadata.img").exists());
    assert!(storage_path.join("data.img").exists());

    // Deinit with purge — pool, loops, and backing files all gone.
    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "deinit",
        "--purge",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "deinit failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !Path::new("/dev/mapper/ember-dead-pool").exists(),
        "ember-dead-pool should be torn down after deinit"
    );
    assert!(!storage_path.join("metadata.img").exists());
    assert!(!storage_path.join("data.img").exists());
}

/// `ember init` should refuse to switch backends silently. After init
/// with one backend, attempting to init with a different backend
/// surfaces a clear error rather than corrupting state.
#[test]
#[ignore = "requires root + dm-thin kernel module"]
fn dm_thin_init_refuses_backend_switch() {
    let tmp = tempfile::tempdir().unwrap();
    let storage_path = tmp.path().join("dm-thin");
    let state_dir = tmp.path().join("state");

    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };

    // First init with dm-thin.
    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--storage",
        "dm-thin",
        "--storage-path",
        storage_path.to_str().unwrap(),
        "--size",
        "200M",
    ]);
    assert!(output.status.success());

    // Second init with zfs should refuse.
    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--storage",
        "zfs",
        "--pool",
        "embertest",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "second init should have failed; stderr: {stderr}"
    );
    assert!(
        stderr.contains("already initialized"),
        "expected 'already initialized' message: {stderr}"
    );
}

/// `ember storage grow --size <larger>` should grow the data device.
#[test]
#[ignore = "requires root + dm-thin kernel module"]
fn dm_thin_storage_grow() {
    let tmp = tempfile::tempdir().unwrap();
    let storage_path = tmp.path().join("dm-thin");
    let state_dir = tmp.path().join("state");

    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };

    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--storage",
        "dm-thin",
        "--storage-path",
        storage_path.to_str().unwrap(),
        "--size",
        "200M",
    ]);
    assert!(output.status.success());

    let initial = std::fs::metadata(storage_path.join("data.img"))
        .unwrap()
        .len();
    assert_eq!(initial, 200 * 1024 * 1024);

    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "storage",
        "grow",
        "--size",
        "400M",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "grow failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    let grown = std::fs::metadata(storage_path.join("data.img"))
        .unwrap()
        .len();
    assert_eq!(grown, 400 * 1024 * 1024);
}
