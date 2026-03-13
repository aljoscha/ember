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

#[allow(dead_code)]
mod common;

#[test]
#[ignore]
fn pull_creates_zvol_and_base_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_and_init("imgpull", &tmp);

    // Pull a small image (alpine is ~3 MB compressed).
    let output = common::ember(&[
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
    common::linux::assert_dataset_exists(&zvol);

    // Verify the @base snapshot exists (used for per-VM cloning).
    common::linux::assert_snapshot_exists(&format!("{zvol}@base"));
}

#[test]
#[ignore]
fn list_shows_pulled_image() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = common::linux::setup_pool_and_init("imglist", &tmp);

    // Pull an image first.
    let pull_output = common::ember(&[
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
    let list_output = common::ember(&["--state-dir", state_dir.to_str().unwrap(), "image", "list"]);
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
    let json_output = common::ember(&[
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
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_and_init("imgdel", &tmp);

    // Pull an image.
    let pull_output = common::ember(&[
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
    common::linux::assert_dataset_exists(&zvol);

    // Delete the image.
    let del_output = common::ember(&[
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
    common::linux::assert_dataset_absent(&zvol);

    // Image list should be empty.
    let list_output = common::ember(&["--state-dir", state_dir.to_str().unwrap(), "image", "list"]);
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
    let (_pool, state_dir, _cleanup) = common::linux::setup_pool_and_init("imgidempotent", &tmp);

    // First pull.
    let pull1 = common::ember(&[
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
    let pull2 = common::ember(&[
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
