//! Integration tests for `ember init` (Linux-only).
//!
//! These tests require root privileges and a working ZFS installation.
#![cfg(target_os = "linux")]
//! They are marked `#[ignore]` so `cargo test` skips them by default.
//!
//! To run:
//!   ./run-integration-tests.sh init
//!
//! The tests create and destroy temporary ZFS pools backed by loopback
//! devices, so they are safe to run on a system with ZFS installed.

#[allow(dead_code)]
mod common;

#[test]
#[ignore]
fn init_creates_new_pool_and_datasets() {
    let pool = common::linux::test_pool("newpool");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = common::linux::create_loop_device(tmp.path());

    let _cleanup = common::linux::PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--pool",
        &pool,
        "--device",
        &loop_dev,
    ]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "init failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Verify ZFS pool and datasets were created.
    common::linux::assert_pool_exists(&pool);
    common::linux::assert_dataset_exists(&format!("{pool}/ember"));
    common::linux::assert_dataset_exists(&format!("{pool}/ember/images"));
    common::linux::assert_dataset_exists(&format!("{pool}/ember/vms"));

    // Verify state directory structure.
    assert!(state_dir.join("kernels").is_dir());
    assert!(state_dir.join("images").is_dir());
    assert!(state_dir.join("vms").is_dir());
    assert!(state_dir.join("network").is_dir());

    // Verify config.json was written with correct content.
    let config_str = std::fs::read_to_string(state_dir.join("config.json")).unwrap();
    let config: serde_json::Value = serde_json::from_str(&config_str).unwrap();
    assert_eq!(config["pool"], pool);
    assert_eq!(config["dataset"], "ember");

    // Stdout should indicate success.
    assert!(stdout.contains("ember initialized successfully"));
}

#[test]
#[ignore]
fn init_idempotent_with_existing_pool() {
    let pool = common::linux::test_pool("existing");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = common::linux::create_loop_device(tmp.path());

    let _cleanup = common::linux::PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    // First init — creates everything.
    let output1 = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--pool",
        &pool,
        "--device",
        &loop_dev,
    ]);
    assert!(
        output1.status.success(),
        "first init failed: {}",
        String::from_utf8_lossy(&output1.stderr)
    );

    // Second init — pool and datasets already exist, should succeed.
    // Note: no --device needed since pool already exists.
    let output2 = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--pool",
        &pool,
    ]);

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    let stderr2 = String::from_utf8_lossy(&output2.stderr);
    assert!(
        output2.status.success(),
        "second init failed.\nstdout: {stdout2}\nstderr: {stderr2}"
    );

    // Should report existing pool and datasets.
    assert!(
        stdout2.contains("already exists"),
        "expected 'already exists' in: {stdout2}"
    );

    // Everything should still be intact.
    common::linux::assert_pool_exists(&pool);
    common::linux::assert_dataset_exists(&format!("{pool}/ember"));
    common::linux::assert_dataset_exists(&format!("{pool}/ember/images"));
    common::linux::assert_dataset_exists(&format!("{pool}/ember/vms"));
    assert!(stdout2.contains("ember initialized successfully"));
}

#[test]
#[ignore]
fn init_fails_without_device_when_pool_missing() {
    let pool = common::linux::test_pool("nodevice");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");

    // No cleanup needed — pool should never be created.

    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--pool",
        &pool,
    ]);

    assert!(
        !output.status.success(),
        "expected init to fail without --device"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist") && stderr.contains("--device"),
        "expected helpful error about --device, got: {stderr}"
    );
}

#[test]
#[ignore]
fn init_custom_dataset_name() {
    let pool = common::linux::test_pool("customds");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = common::linux::create_loop_device(tmp.path());

    let _cleanup = common::linux::PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--pool",
        &pool,
        "--device",
        &loop_dev,
        "--dataset",
        "mydata",
    ]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "init failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Verify custom dataset name was used.
    common::linux::assert_dataset_exists(&format!("{pool}/mydata"));
    common::linux::assert_dataset_exists(&format!("{pool}/mydata/images"));
    common::linux::assert_dataset_exists(&format!("{pool}/mydata/vms"));

    // Config should reflect custom dataset name.
    let config_str = std::fs::read_to_string(state_dir.join("config.json")).unwrap();
    let config: serde_json::Value = serde_json::from_str(&config_str).unwrap();
    assert_eq!(config["dataset"], "mydata");
}
