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

use std::path::PathBuf;
use std::process::Command;

/// Unique pool name per test to avoid collisions.
fn test_pool(name: &str) -> String {
    format!("embertest_{name}_{}", std::process::id())
}

/// Create a loopback file and attach it to a loop device.
/// Returns (loop_device_path, backing_file_path).
fn create_loop_device(dir: &std::path::Path) -> (String, PathBuf) {
    let file = dir.join("pool.img");

    // Create a 64MB sparse file for the pool.
    let status = Command::new("truncate")
        .args(["-s", "64M"])
        .arg(&file)
        .status()
        .expect("failed to run truncate");
    assert!(status.success(), "truncate failed");

    let output = Command::new("losetup")
        .args(["--find", "--show"])
        .arg(&file)
        .output()
        .expect("failed to run losetup");
    assert!(
        output.status.success(),
        "losetup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dev = String::from_utf8(output.stdout).unwrap().trim().to_string();
    (dev, file)
}

/// Detach a loop device (best-effort cleanup).
fn detach_loop_device(dev: &str) {
    let _ = Command::new("losetup").args(["-d", dev]).status();
}

/// Destroy a ZFS pool (best-effort cleanup).
fn destroy_pool(pool: &str) {
    let _ = Command::new("zpool").args(["destroy", "-f", pool]).status();
}

/// Path to the ember binary built by cargo.
fn ember_bin() -> PathBuf {
    // `cargo test` puts the test binary in target/debug/deps, but the
    // main binary is at target/debug/ember.
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    if path.ends_with("deps") {
        path.pop(); // remove deps/
    }
    path.push("ember");
    path
}

/// Run ember with the given args, returning the Output.
fn ember(args: &[&str]) -> std::process::Output {
    Command::new(ember_bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute ember: {e}"))
}

/// Assert that a ZFS pool exists.
fn assert_pool_exists(pool: &str) {
    let output = Command::new("zpool")
        .args(["list", "-H", pool])
        .output()
        .expect("failed to run zpool");
    assert!(output.status.success(), "expected pool '{pool}' to exist");
}

/// Assert that a ZFS dataset exists.
fn assert_dataset_exists(dataset: &str) {
    let output = Command::new("zfs")
        .args(["list", "-H", dataset])
        .output()
        .expect("failed to run zfs");
    assert!(
        output.status.success(),
        "expected dataset '{dataset}' to exist"
    );
}

#[test]
#[ignore]
fn init_creates_new_pool_and_datasets() {
    let pool = test_pool("newpool");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = create_loop_device(tmp.path());

    // Cleanup on exit (even if test panics).
    struct Cleanup {
        pool: String,
        dev: String,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            destroy_pool(&self.pool);
            detach_loop_device(&self.dev);
        }
    }
    let _cleanup = Cleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    let output = ember(&[
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
    assert_pool_exists(&pool);
    assert_dataset_exists(&format!("{pool}/ember"));
    assert_dataset_exists(&format!("{pool}/ember/images"));
    assert_dataset_exists(&format!("{pool}/ember/vms"));

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
    let pool = test_pool("existing");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = create_loop_device(tmp.path());

    struct Cleanup {
        pool: String,
        dev: String,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            destroy_pool(&self.pool);
            detach_loop_device(&self.dev);
        }
    }
    let _cleanup = Cleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    // First init — creates everything.
    let output1 = ember(&[
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
    let output2 = ember(&[
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
    assert_pool_exists(&pool);
    assert_dataset_exists(&format!("{pool}/ember"));
    assert_dataset_exists(&format!("{pool}/ember/images"));
    assert_dataset_exists(&format!("{pool}/ember/vms"));
    assert!(stdout2.contains("ember initialized successfully"));
}

#[test]
#[ignore]
fn init_fails_without_device_when_pool_missing() {
    let pool = test_pool("nodevice");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");

    // No cleanup needed — pool should never be created.

    let output = ember(&[
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
    let pool = test_pool("customds");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = create_loop_device(tmp.path());

    struct Cleanup {
        pool: String,
        dev: String,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            destroy_pool(&self.pool);
            detach_loop_device(&self.dev);
        }
    }
    let _cleanup = Cleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    let output = ember(&[
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
    assert_dataset_exists(&format!("{pool}/mydata"));
    assert_dataset_exists(&format!("{pool}/mydata/images"));
    assert_dataset_exists(&format!("{pool}/mydata/vms"));

    // Config should reflect custom dataset name.
    let config_str = std::fs::read_to_string(state_dir.join("config.json")).unwrap();
    let config: serde_json::Value = serde_json::from_str(&config_str).unwrap();
    assert_eq!(config["dataset"], "mydata");
}
