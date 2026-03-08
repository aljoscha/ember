//! Integration tests for `ember image pull`, `image list`, and `image delete` (Linux-only).
//!
//! These tests require:
#![cfg(target_os = "linux")]
//!
//! - Root privileges
//! - Working ZFS installation
//! - Network access (to pull OCI images from Docker Hub)
//! - `skopeo` installed
//!
//! They are marked `#[ignore]` so `cargo test` skips them by default.
//!
//! To run:
//!   ./run-integration-tests.sh image

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

    // 512 MB sparse file — images need space for the zvol + ext4 overhead.
    let status = Command::new("truncate")
        .args(["-s", "512M"])
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

/// RAII guard: destroys pool and detaches loop device on drop.
struct PoolCleanup {
    pool: String,
    dev: String,
}

impl Drop for PoolCleanup {
    fn drop(&mut self) {
        destroy_pool(&self.pool);
        detach_loop_device(&self.dev);
    }
}

/// Assert that a ZFS dataset (zvol, filesystem, etc.) exists.
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

/// Assert that a ZFS snapshot exists.
fn assert_snapshot_exists(snapshot: &str) {
    let output = Command::new("zfs")
        .args(["list", "-t", "snapshot", "-H", snapshot])
        .output()
        .expect("failed to run zfs");
    assert!(
        output.status.success(),
        "expected snapshot '{snapshot}' to exist"
    );
}

/// Assert that a ZFS dataset does NOT exist.
fn assert_dataset_absent(dataset: &str) {
    let output = Command::new("zfs")
        .args(["list", "-H", dataset])
        .output()
        .expect("failed to run zfs");
    assert!(
        !output.status.success(),
        "expected dataset '{dataset}' to NOT exist"
    );
}

/// Set up a ZFS pool and run `ember init`. Returns (pool_name, state_dir, cleanup_guard).
fn setup_pool_and_init(test_name: &str, tmp: &tempfile::TempDir) -> (String, PathBuf, PoolCleanup) {
    let pool = test_pool(test_name);
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = create_loop_device(tmp.path());

    let cleanup = PoolCleanup {
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

    (pool, state_dir, cleanup)
}

#[test]
#[ignore]
fn pull_creates_zvol_and_base_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = setup_pool_and_init("imgpull", &tmp);

    // Pull a small image (alpine is ~3 MB compressed).
    let output = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "pull",
        "alpine:latest",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "image pull failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("pulled successfully"),
        "expected success message in stdout: {stdout}"
    );

    // Verify the ZFS zvol was created.
    let zvol = format!("{pool}/ember/images/library-alpine-latest");
    assert_dataset_exists(&zvol);

    // Verify the @base snapshot exists (used for per-VM cloning).
    assert_snapshot_exists(&format!("{zvol}@base"));
}

#[test]
#[ignore]
fn list_shows_pulled_image() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_and_init("imglist", &tmp);

    // Pull an image first.
    let pull_output = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "pull",
        "alpine:latest",
    ]);
    assert!(
        pull_output.status.success(),
        "image pull failed: {}",
        String::from_utf8_lossy(&pull_output.stderr)
    );

    // Table output should contain the image.
    let list_output = ember(&["--state-dir", state_dir.to_str().unwrap(), "image", "list"]);
    let stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_output.status.success(),
        "image list failed: {}",
        String::from_utf8_lossy(&list_output.stderr)
    );
    assert!(
        stdout.contains("library-alpine-latest"),
        "expected 'library-alpine-latest' in table output: {stdout}"
    );
    assert!(
        stdout.contains("docker.io/library/alpine:latest"),
        "expected full reference in table output: {stdout}"
    );

    // JSON output should contain structured image data.
    let json_output = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "list",
        "--format",
        "json",
    ]);
    let json_stdout = String::from_utf8_lossy(&json_output.stdout);
    assert!(json_output.status.success());

    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|e| panic!("invalid JSON from image list: {e}\noutput: {json_stdout}"));
    let images = parsed["images"]
        .as_array()
        .expect("expected 'images' array in JSON output");
    assert_eq!(images.len(), 1, "expected exactly one image in list");
    assert_eq!(images[0]["local_name"], "library-alpine-latest");
    assert_eq!(images[0]["reference"], "docker.io/library/alpine:latest");
}

#[test]
#[ignore]
fn delete_removes_image_and_zvol() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = setup_pool_and_init("imgdel", &tmp);

    // Pull an image.
    let pull_output = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "pull",
        "alpine:latest",
    ]);
    assert!(
        pull_output.status.success(),
        "image pull failed: {}",
        String::from_utf8_lossy(&pull_output.stderr)
    );

    let zvol = format!("{pool}/ember/images/library-alpine-latest");
    assert_dataset_exists(&zvol);

    // Delete the image.
    let del_output = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "delete",
        "alpine:latest",
    ]);
    let stdout = String::from_utf8_lossy(&del_output.stdout);
    let stderr = String::from_utf8_lossy(&del_output.stderr);
    assert!(
        del_output.status.success(),
        "image delete failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // ZFS zvol and snapshot should be gone.
    assert_dataset_absent(&zvol);

    // Image list should be empty.
    let list_output = ember(&["--state-dir", state_dir.to_str().unwrap(), "image", "list"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_stdout.contains("No images found"),
        "expected empty image list, got: {list_stdout}"
    );
}

#[test]
#[ignore]
fn pull_same_image_twice_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_and_init("imgidempotent", &tmp);

    // First pull.
    let pull1 = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "pull",
        "alpine:latest",
    ]);
    assert!(
        pull1.status.success(),
        "first pull failed: {}",
        String::from_utf8_lossy(&pull1.stderr)
    );

    // Second pull of the same image.
    let pull2 = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "pull",
        "alpine:latest",
    ]);
    let stdout2 = String::from_utf8_lossy(&pull2.stdout);
    assert!(
        pull2.status.success(),
        "second pull failed: {}",
        String::from_utf8_lossy(&pull2.stderr)
    );

    // Second pull should report the image already exists without re-pulling.
    assert!(
        stdout2.contains("already exists"),
        "expected 'already exists' in second pull output: {stdout2}"
    );
}
