//! Integration tests for `ember image pull/list/delete` on macOS.
//!
//! These tests verify the full image pipeline on macOS:
//! - OCI pull via skopeo → ext4 creation via hdiutil → APFS storage
//! - Image listing (table + JSON output)
//! - Image deletion (registry + file cleanup)
//! - Idempotent re-pull of the same image
//!
//! Requirements:
//! - macOS with APFS filesystem
//! - Homebrew `e2fsprogs` and `skopeo`
//! - Network access (pulls from Docker Hub)
//! - No root required
//!
//! To run:
//!   ./run-integration-tests.sh macos_image
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Path to the ember binary built by cargo.
fn ember_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
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

/// Run `ember init` with a temporary state directory.
fn setup_init(tmp: &Path) -> PathBuf {
    let state_dir = tmp.join("state");
    let output = ember(&["--state-dir", state_dir.to_str().unwrap(), "init"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "init failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    state_dir
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full end-to-end: pull alpine, verify image file exists, list shows it.
#[test]
#[ignore]
fn pull_alpine_creates_image_file() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_init(tmp.path());

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

    // Verify the .img file was created in the images/data/ directory.
    let img_path = state_dir.join("images/data/library-alpine-latest.img");
    assert!(
        img_path.exists(),
        "expected image file at {}",
        img_path.display()
    );

    // The image file should be a reasonable size (at least 10 MiB for Alpine).
    let metadata = std::fs::metadata(&img_path).unwrap();
    assert!(
        metadata.len() >= 10 * 1024 * 1024,
        "image file too small: {} bytes",
        metadata.len()
    );
}

/// Image list shows the pulled image in both table and JSON formats.
#[test]
#[ignore]
fn list_shows_pulled_image() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_init(tmp.path());

    // Pull first.
    let pull = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "pull",
        "alpine:latest",
    ]);
    assert!(
        pull.status.success(),
        "image pull failed: {}",
        String::from_utf8_lossy(&pull.stderr)
    );

    // Table output should contain the image.
    let list = ember(&["--state-dir", state_dir.to_str().unwrap(), "image", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(list.status.success());
    assert!(
        stdout.contains("library-alpine-latest"),
        "expected local name in table output: {stdout}"
    );
    assert!(
        stdout.contains("docker.io/library/alpine:latest"),
        "expected full reference in table output: {stdout}"
    );

    // JSON output should contain structured image data.
    let json_list = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "list",
        "--format",
        "json",
    ]);
    let json_stdout = String::from_utf8_lossy(&json_list.stdout);
    assert!(json_list.status.success());

    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {json_stdout}"));
    let images = parsed["images"]
        .as_array()
        .expect("expected 'images' array");
    assert_eq!(images.len(), 1, "expected one image");
    assert_eq!(images[0]["local_name"], "library-alpine-latest");
}

/// Deleting an image removes both the registry entry and the .img file.
#[test]
#[ignore]
fn delete_removes_image_and_file() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_init(tmp.path());

    // Pull.
    let pull = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "pull",
        "alpine:latest",
    ]);
    assert!(pull.status.success());

    let img_path = state_dir.join("images/data/library-alpine-latest.img");
    assert!(img_path.exists());

    // Delete.
    let del = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "delete",
        "alpine:latest",
    ]);
    let stdout = String::from_utf8_lossy(&del.stdout);
    let stderr = String::from_utf8_lossy(&del.stderr);
    assert!(
        del.status.success(),
        "image delete failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // .img file should be gone.
    assert!(
        !img_path.exists(),
        "image file should have been deleted: {}",
        img_path.display()
    );

    // Image list should be empty.
    let list = ember(&["--state-dir", state_dir.to_str().unwrap(), "image", "list"]);
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("No images found"),
        "expected empty list, got: {list_stdout}"
    );
}

/// Pulling the same image twice is idempotent (no re-download).
#[test]
#[ignore]
fn pull_same_image_twice_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_init(tmp.path());

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

    // Second pull should report it already exists.
    let pull2 = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "pull",
        "alpine:latest",
    ]);
    let stdout2 = String::from_utf8_lossy(&pull2.stdout);
    assert!(pull2.status.success());
    assert!(
        stdout2.contains("already exists"),
        "expected 'already exists' on re-pull: {stdout2}"
    );
}
