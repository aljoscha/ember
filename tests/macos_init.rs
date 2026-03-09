//! Integration tests for `ember init` on macOS.
//!
//! These tests verify:
//! - `ember init` creates the correct directory structure
//! - `config.json` is written with expected fields
//! - Re-running `ember init` is idempotent
//! - No root required
//!
//! Requirements:
//! - macOS with APFS filesystem
//! - No root required
//!
//! To run:
//!   ./run-integration-tests.sh macos_init
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

/// Run `ember init` with a temporary state directory, returning the state dir path.
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

/// `ember init` creates the expected directory structure for macOS.
#[test]
#[ignore]
fn init_creates_directory_structure() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_init(tmp.path());

    // Required directories.
    let expected_dirs = [
        "images/data",
        "vms",
        "kernels",
        "network",
    ];

    for dir in &expected_dirs {
        let path = state_dir.join(dir);
        assert!(
            path.is_dir(),
            "expected directory to exist: {}",
            path.display()
        );
    }
}

/// `ember init` writes a valid config.json.
#[test]
#[ignore]
fn init_writes_config_json() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_init(tmp.path());

    let config_path = state_dir.join("config.json");
    assert!(config_path.exists(), "config.json not found");

    let content = std::fs::read_to_string(&config_path).unwrap();
    let config: serde_json::Value =
        serde_json::from_str(&content).expect("config.json is not valid JSON");

    // state_dir field should be present and match.
    let stored_state_dir = config["state_dir"].as_str().unwrap();
    assert_eq!(
        stored_state_dir,
        state_dir.to_str().unwrap(),
        "state_dir in config.json doesn't match"
    );
}

/// Re-running `ember init` is idempotent (doesn't fail or overwrite data).
#[test]
#[ignore]
fn init_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_init(tmp.path());

    // Run init again on the same state directory.
    let output = ember(&["--state-dir", state_dir.to_str().unwrap(), "init"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "second init failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Directories should still exist.
    assert!(state_dir.join("images/data").is_dir());
    assert!(state_dir.join("vms").is_dir());
}

/// `ember init` does not require root on macOS.
#[test]
#[ignore]
fn init_works_without_root() {
    // This test itself runs without root. If we get here and init succeeds,
    // that proves no root is needed.
    assert!(
        !nix::unistd::geteuid().is_root(),
        "this test should run as a non-root user"
    );

    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_init(tmp.path());

    // Verify basic structure was created.
    assert!(state_dir.join("config.json").exists());
    assert!(state_dir.join("vms").is_dir());
}
