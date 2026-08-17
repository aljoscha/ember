//! Integration tests for macOS APFS storage backend.
//!
//! These tests verify macOS-specific storage behaviors:
//! - APFS CoW clone space efficiency (clones don't consume extra space)
//! - Space accounting via `ember storage usage`
//! - VM delete removes storage
//! - Non-APFS (HFS+) detection and warnings
//!
//! Cross-platform resize tests live in `resize.rs`.
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

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
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

/// `ember storage usage` should report images and VMs.
#[test]
#[ignore]
fn storage_usage_reports_images_and_vms() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = common::macos::setup_init(tmp.path());
    let state = state_dir.to_str().unwrap();
    let img = common::macos::create_test_image(tmp.path(), "efftest", 64);
    common::macos::register_test_image(&state_dir, "effimg", "latest", &img);

    for i in 0..3 {
        let vm_name = format!("effvm{i}");
        common::macos::create_test_vm_manual(&state_dir, &vm_name, "effimg-latest");

        // Fork each VM to exercise a second layer of CoW (image → VM → fork).
        let fork_name = format!("efffork{i}");
        let output = common::ember(&[
            "--state-dir",
            state,
            "vm",
            "fork",
            &vm_name,
            &fork_name,
            "--no-start",
        ]);
        assert!(
            output.status.success(),
            "vm fork failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = common::ember(&["--state-dir", state, "storage", "usage"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "storage usage failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(stdout.contains("Pool"), "expected a pool line in: {stdout}");
    assert!(stdout.contains("IMAGES"), "expected images in: {stdout}");
    // The three VMs and their three forks.
    for i in 0..3 {
        assert!(
            stdout.contains(&format!("effvm{i}")),
            "expected effvm{i} in: {stdout}"
        );
        assert!(
            stdout.contains(&format!("efffork{i}")),
            "expected efffork{i} in: {stdout}"
        );
    }
}

/// VM delete should remove all storage (rootfs + VM directory).
#[test]
#[ignore]
fn vm_delete_removes_storage() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = common::macos::setup_with_vm(tmp.path(), "deltest", "delvm");
    let state = state_dir.to_str().unwrap();

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

/// Verify that ember warns about non-APFS volumes and that the
/// `clonefile(2)` mechanism it relies on surfaces a non-CoW situation.
#[test]
#[ignore]
fn clone_fails_gracefully_on_non_apfs() {
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

    // ember clones via `clonefile(2)` directly, not `cp -c` — modern `cp -c`
    // silently falls back to a full copy across volumes, which would hide a
    // misconfigured non-APFS state directory. Verify the syscall ember relies
    // on does surface the cross-volume case (EXDEV) or an unsupported
    // filesystem (ENOTSUP), rather than succeeding.
    let img = common::macos::create_test_image(tmp.path(), "hfstest", 8);
    let cross_vol_dest = PathBuf::from(&mount_point).join("cross-vol-clone.img");

    let src_c = CString::new(img.as_os_str().as_bytes()).unwrap();
    let dst_c = CString::new(cross_vol_dest.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { nix::libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    assert_eq!(rc, -1, "clonefile should fail across volumes");
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert!(
        errno == Some(nix::libc::EXDEV) || errno == Some(nix::libc::ENOTSUP),
        "expected EXDEV or ENOTSUP for cross-volume clonefile, got {errno:?}"
    );
}
