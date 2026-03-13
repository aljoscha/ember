//! Integration tests for macOS APFS storage backend.
//!
//! These tests verify macOS-specific storage behaviors:
//! - APFS CoW clone space efficiency (cp -c doesn't consume extra space)
//! - Storage efficiency debug command
//! - VM delete removes storage
//! - Non-APFS (HFS+) detection and warnings
//!
//! Cross-platform snapshot and resize tests live in `snapshot.rs` and
//! `resize.rs` respectively.
//!
//! Requirements:
//! - macOS with APFS filesystem (default since 10.13)
//! - Homebrew `e2fsprogs` (for mkfs.ext4)
//! - No root required
//!
//! To run:
//!   ./run-integration-tests.sh macos_storage
#![cfg(target_os = "macos")]

#[allow(dead_code)]
mod common;

use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// APFS-specific tests
// ---------------------------------------------------------------------------

/// Verify that `cp -c` APFS clone doesn't reduce free space significantly.
///
/// Creates a 64MB base image, clones it 5 times, then checks that
/// disk usage increase is much less than 5 * 64MB (proving CoW works).
#[test]
#[ignore]
fn apfs_clone_does_not_reduce_free_space() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = common::macos::setup_init(tmp.path());
    let img = common::macos::create_test_image(tmp.path(), "cowtest", 64);
    common::macos::register_test_image(&state_dir, "cowimg", "latest", &img);

    let free_before = common::macos::get_free_space_bytes(tmp.path());

    for i in 0..5 {
        let vm_name = format!("cowvm{i}");
        common::macos::create_test_vm_manual(&state_dir, &vm_name, "cowimg-latest");
    }

    let free_after = common::macos::get_free_space_bytes(tmp.path());

    let consumed = free_before.saturating_sub(free_after);
    let max_expected = 5 * 1024 * 1024; // 5MB
    assert!(
        consumed < max_expected,
        "CoW clones consumed {consumed} bytes — expected less than {max_expected}. \
         This suggests cp -c is doing full copies instead of CoW clones."
    );
}

/// `ember debug storage-efficiency` should report images, VMs, and snapshots.
#[test]
#[ignore]
fn storage_efficiency_shows_savings() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = common::macos::setup_init(tmp.path());
    let state = state_dir.to_str().unwrap();
    let img = common::macos::create_test_image(tmp.path(), "efftest", 64);
    common::macos::register_test_image(&state_dir, "effimg", "latest", &img);

    for i in 0..3 {
        let vm_name = format!("effvm{i}");
        common::macos::create_test_vm_manual(&state_dir, &vm_name, "effimg-latest");

        let output = common::ember(&[
            "--state-dir",
            state,
            "snapshot",
            "create",
            &vm_name,
            "snap1",
        ]);
        assert!(
            output.status.success(),
            "snapshot create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = common::ember(&["--state-dir", state, "debug", "storage-efficiency"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "storage-efficiency failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(
        stdout.contains("Images:"),
        "expected 'Images:' in: {stdout}"
    );
    assert!(stdout.contains("VMs:"), "expected 'VMs:' in: {stdout}");
    assert!(
        stdout.contains("Snapshots:"),
        "expected 'Snapshots:' in: {stdout}"
    );
    assert!(
        stdout.contains("Total logical:"),
        "expected 'Total logical:' in: {stdout}"
    );
}

/// VM delete should remove all storage (rootfs + snapshots directory).
#[test]
#[ignore]
fn vm_delete_removes_storage() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = common::macos::setup_with_vm(tmp.path(), "deltest", "delvm");
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&["--state-dir", state, "snapshot", "create", "delvm", "snap1"]);
    assert!(output.status.success());

    let vm_dir = state_dir.join("vms").join("delvm");
    assert!(vm_dir.exists(), "VM dir should exist before delete");

    let output = common::ember(&["--state-dir", state, "vm", "delete", "delvm"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm delete failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(!vm_dir.exists(), "VM dir should not exist after delete");
}

// ---------------------------------------------------------------------------
// Non-APFS failure tests
// ---------------------------------------------------------------------------

/// Verify that ember warns about non-APFS volumes and detects missing CoW.
#[test]
#[ignore]
fn cp_c_fails_gracefully_on_non_apfs() {
    let tmp = tempfile::tempdir().unwrap();
    let dmg_path = tmp.path().join("hfsplus.dmg");

    let output = Command::new("hdiutil")
        .args([
            "create",
            "-size",
            "64m",
            "-fs",
            "HFS+",
            "-volname",
            "EmberTestHFS",
        ])
        .arg(&dmg_path)
        .output()
        .expect("failed to run hdiutil create");
    assert!(
        output.status.success(),
        "hdiutil create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-plist"])
        .arg(&dmg_path)
        .output()
        .expect("failed to run hdiutil attach");
    assert!(
        output.status.success(),
        "hdiutil attach failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mount_point = common::macos::parse_hdiutil_mount_point(&stdout)
        .unwrap_or_else(|| panic!("no mount point found in hdiutil output:\n{stdout}"));

    struct HdiutilCleanup(String);
    impl Drop for HdiutilCleanup {
        fn drop(&mut self) {
            let _ = Command::new("hdiutil")
                .args(["detach", "-force"])
                .arg(&self.0)
                .status();
        }
    }
    let _cleanup = HdiutilCleanup(mount_point.clone());

    let state_dir = PathBuf::from(&mount_point).join("ember-state");

    let output = common::ember(&["--state-dir", state_dir.to_str().unwrap(), "init"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "init on HFS+ failed (should warn, not error).\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("not on an APFS volume") || stderr.contains("not APFS"),
        "expected APFS warning during init on HFS+ volume.\nstderr: {stderr}"
    );

    let img = common::macos::create_test_image(tmp.path(), "hfstest", 8);
    let cross_vol_dest = PathBuf::from(&mount_point).join("cross-vol-clone.img");
    let cp_output = Command::new("cp")
        .arg("-c")
        .arg(&img)
        .arg(&cross_vol_dest)
        .output()
        .expect("failed to run cp -c");
    assert!(
        !cp_output.status.success(),
        "cp -c should fail for cross-volume clone"
    );
    let cp_stderr = String::from_utf8_lossy(&cp_output.stderr);
    assert!(
        cp_stderr.contains("Cross-device link"),
        "expected 'Cross-device link' error for cross-volume cp -c.\nstderr: {cp_stderr}"
    );
}
