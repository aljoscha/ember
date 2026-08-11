//! Cross-installation isolation contract.
//!
//! These tests guard the promise that two ember installations on the
//! same host don't see, share, or destroy each other's resources, and
//! that an installation predating `instance_id` keeps working after
//! the binary upgrade. Failures here mean an integration-test run can
//! corrupt the developer's live install — exactly what `instance_id`
//! exists to prevent.
//!
//! Gated `#[ignore]` and Linux-only because they touch real
//! device-mapper, TAP, and iptables state. Run explicitly with:
//!
//! ```text
//! sudo cargo test --test isolation -- --ignored --test-threads=1
//! ```

#![cfg(target_os = "linux")]

#[allow(dead_code)]
mod common;

use std::path::Path;

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

/// `deinit` must only remove firewall chains belonging to *this*
/// install. Create install A's chains manually, tear down install B,
/// and verify A's chains are still there. Chains are host-global and
/// named per install, so an unscoped delete would take down the
/// developer's live policy from inside a test run.
#[test]
#[ignore = "requires root + iptables"]
fn deinit_does_not_touch_other_installs_chains() {
    let tmp = tempfile::tempdir().unwrap();
    let storage_b = tmp.path().join("dm-thin-b");
    let state_b = tmp.path().join("state-b");

    let _cleanup_b = common::linux::DmThinCleanup {
        state_dir: state_b.clone(),
    };

    // Stand in for install A: the chain names `instance_id` "aaaa"
    // would derive, created directly so the test doesn't have to boot a
    // VM to bring them into existence.
    const CHAINS_A: [&str; 2] = ["ember-aaaa-input", "ember-aaaa-forward"];
    struct ChainCleanup;
    impl Drop for ChainCleanup {
        fn drop(&mut self) {
            for chain in CHAINS_A {
                let _ = std::process::Command::new("iptables")
                    .args(["-w", "5", "-X", chain])
                    .status();
            }
        }
    }
    let _chain_cleanup = ChainCleanup;
    for chain in CHAINS_A {
        let status = std::process::Command::new("iptables")
            .args(["-w", "5", "-N", chain])
            .status()
            .expect("failed to run iptables -N");
        assert!(status.success(), "failed to create test chain {chain}");
    }

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

    let output = common::ember(&[
        "--state-dir",
        state_b.to_str().unwrap(),
        "deinit",
        "--purge",
    ]);
    assert!(
        output.status.success(),
        "deinit B failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for chain in CHAINS_A {
        let check = std::process::Command::new("iptables")
            .args(["-w", "5", "-S", chain])
            .output()
            .expect("failed to run iptables -S");
        assert!(
            check.status.success(),
            "install B's deinit deleted install A's chain '{chain}'"
        );
    }
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
