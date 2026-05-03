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

/// Two installs at different `--state-dir` with distinct
/// `--instance-id`s must not share dm-thin pools, and tearing one
/// down must leave the other intact. This is the core promise that
/// `instance_id` exists to deliver.
#[test]
#[ignore = "requires root + dm-thin kernel module"]
fn dm_thin_two_installs_dont_interfere() {
    let tmp = tempfile::tempdir().unwrap();

    let storage_a = tmp.path().join("dm-thin-a");
    let state_a = tmp.path().join("state-a");
    let storage_b = tmp.path().join("dm-thin-b");
    let state_b = tmp.path().join("state-b");

    // Order matters: cleanup runs in reverse, so install A is torn
    // down last. If install B's deinit accidentally killed pool A,
    // the assertions below catch it before this fires.
    let _cleanup_b = common::linux::DmThinCleanup {
        state_dir: state_b.clone(),
    };
    let _cleanup_a = common::linux::DmThinCleanup {
        state_dir: state_a.clone(),
    };

    // Init install A.
    let output = common::ember(&[
        "--state-dir",
        state_a.to_str().unwrap(),
        "init",
        "--storage",
        "dm-thin",
        "--storage-path",
        storage_a.to_str().unwrap(),
        "--size",
        "200M",
        "--instance-id",
        "aaaa",
    ]);
    assert!(
        output.status.success(),
        "init A failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Init install B. Different instance id — pool name must not
    // collide.
    let output = common::ember(&[
        "--state-dir",
        state_b.to_str().unwrap(),
        "init",
        "--storage",
        "dm-thin",
        "--storage-path",
        storage_b.to_str().unwrap(),
        "--size",
        "200M",
        "--instance-id",
        "bbbb",
    ]);
    assert!(
        output.status.success(),
        "init B failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Both pools live side-by-side in the kernel.
    assert!(
        Path::new("/dev/mapper/ember-aaaa-pool").exists(),
        "ember-aaaa-pool should exist after install A's init"
    );
    assert!(
        Path::new("/dev/mapper/ember-bbbb-pool").exists(),
        "ember-bbbb-pool should exist after install B's init"
    );

    // Tear down install A. Install B must survive untouched — this
    // is the regression guard against the old singleton pool name.
    let output = common::ember(&[
        "--state-dir",
        state_a.to_str().unwrap(),
        "deinit",
        "--purge",
    ]);
    assert!(
        output.status.success(),
        "deinit A failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !Path::new("/dev/mapper/ember-aaaa-pool").exists(),
        "ember-aaaa-pool should be gone after install A's deinit"
    );
    assert!(
        Path::new("/dev/mapper/ember-bbbb-pool").exists(),
        "ember-bbbb-pool must NOT have been torn down by install A's deinit"
    );

    // Confirm install B is still functional: deinit it cleanly.
    let output = common::ember(&[
        "--state-dir",
        state_b.to_str().unwrap(),
        "deinit",
        "--purge",
    ]);
    assert!(
        output.status.success(),
        "deinit B failed (would indicate install A's deinit corrupted B): {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!Path::new("/dev/mapper/ember-bbbb-pool").exists());
}

/// Configs written by older ember binaries (no `instance_id` field)
/// must keep working without forcing a deinit/reinit. Verifies the
/// binary deserializes such a config and emits the legacy unprefixed
/// pool name through the public `ember info` surface.
#[test]
#[ignore = "requires root (writes to a temp state dir but doesn't touch the kernel)"]
fn legacy_config_without_instance_id_keeps_working() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    // Recreate the directory layout that `StateStore::init()` would
    // have produced. `try_open` keys off the presence of `vms/`.
    for sub in ["", "vms", "images", "network", "kernels"] {
        std::fs::create_dir_all(state_dir.join(sub)).unwrap();
    }

    // Hand-craft a config exactly like an older binary would write
    // it: storage_backend present, no instance_id, no ip_subnet.
    let legacy_config = serde_json::json!({
        "storage_backend": "dm-thin",
        "pool": "tank",
        "dataset": "ember",
        "kernel_path": null,
        "wan_iface": null,
        "state_dir": state_dir.to_str().unwrap(),
        "storage_path": tmp.path().join("dm-thin").to_str().unwrap(),
        "dm_thin_block_size": 128,
        "dm_thin_mode": "file",
    });
    std::fs::write(
        state_dir.join("config.json"),
        serde_json::to_vec_pretty(&legacy_config).unwrap(),
    )
    .unwrap();

    // `ember info` reads config and renders the dm-thin pool name
    // via the accessor. Legacy mode must report the unprefixed name.
    let output = common::ember(&["--state-dir", state_dir.to_str().unwrap(), "info"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "info on a legacy config failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("ember-pool"),
        "expected legacy pool name 'ember-pool' in info output:\n{stdout}"
    );
    // Make sure the new prefixed form did NOT sneak in.
    assert!(
        !stdout.contains("ember--pool"),
        "legacy compat regression: empty instance_id produced 'ember--pool':\n{stdout}"
    );
}

/// Reconcile (run at the start of every command) must only sweep
/// TAP devices belonging to *this* install's prefix. Create a TAP
/// device manually with install A's prefix, then run a reconcile-
/// triggering command in install B and verify A's TAP survives.
#[test]
#[ignore = "requires root + ip-link"]
fn reconcile_does_not_touch_other_installs_taps() {
    let tmp = tempfile::tempdir().unwrap();
    let storage_a = tmp.path().join("dm-thin-a");
    let state_a = tmp.path().join("state-a");
    let storage_b = tmp.path().join("dm-thin-b");
    let state_b = tmp.path().join("state-b");

    let _cleanup_b = common::linux::DmThinCleanup {
        state_dir: state_b.clone(),
    };
    let _cleanup_a = common::linux::DmThinCleanup {
        state_dir: state_a.clone(),
    };

    // Init both installs with distinct instance ids → distinct TAP
    // prefixes (`emaaaa-` and `embbbb-`).
    for (state, storage, id) in [
        (&state_a, &storage_a, "aaaa"),
        (&state_b, &storage_b, "bbbb"),
    ] {
        let output = common::ember(&[
            "--state-dir",
            state.to_str().unwrap(),
            "init",
            "--storage",
            "dm-thin",
            "--storage-path",
            storage.to_str().unwrap(),
            "--size",
            "200M",
            "--instance-id",
            id,
        ]);
        assert!(
            output.status.success(),
            "init {id} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Manually create a TAP device that looks like one of install A's
    // VMs. We don't bring it up or address it — `ip tuntap add` is
    // enough for the kernel to list it as a tun device, which is what
    // ember's reconcile sweep enumerates.
    let tap_a = "emaaaa-deadbee";
    let status = std::process::Command::new("ip")
        .args(["tuntap", "add", tap_a, "mode", "tap"])
        .status()
        .expect("failed to run ip tuntap add");
    assert!(status.success(), "failed to create test TAP {tap_a}");
    // Always remove the TAP, even on assertion panic below.
    struct TapCleanup(&'static str);
    impl Drop for TapCleanup {
        fn drop(&mut self) {
            let _ = std::process::Command::new("ip")
                .args(["tuntap", "del", self.0, "mode", "tap"])
                .status();
        }
    }
    let _tap_cleanup = TapCleanup(tap_a);

    // Run a reconcile-triggering command in install B. `vm list` is
    // cheap and runs reconcile up-front; if reconcile is wrongly
    // unscoped, it will sweep `emaaaa-deadbee`.
    let output = common::ember(&["--state-dir", state_b.to_str().unwrap(), "vm", "list"]);
    assert!(
        output.status.success(),
        "vm list in install B failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Install A's TAP must still exist.
    let check = std::process::Command::new("ip")
        .args(["link", "show", tap_a])
        .output()
        .expect("failed to run ip link show");
    assert!(
        check.status.success(),
        "install B's reconcile deleted install A's TAP '{tap_a}': {}",
        String::from_utf8_lossy(&check.stderr)
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
