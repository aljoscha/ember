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

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Create a minimal ext4 image file using mkfs.ext4 from Homebrew e2fsprogs.
fn create_test_image(dir: &Path, name: &str, size_mb: u64) -> PathBuf {
    let img = dir.join(format!("{name}.img"));

    let status = Command::new("truncate")
        .args(["-s", &format!("{size_mb}M")])
        .arg(&img)
        .status()
        .expect("failed to run truncate");
    assert!(status.success(), "truncate failed");

    let mkfs = common::macos::find_e2fsprogs_tool("mkfs.ext4");
    let output = Command::new(&mkfs)
        .args(["-F", "-q"])
        .arg(&img)
        .output()
        .unwrap_or_else(|_| {
            panic!("failed to run {mkfs} — is e2fsprogs installed? (brew install e2fsprogs)")
        });
    assert!(
        output.status.success(),
        "mkfs.ext4 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    img
}

/// Register a test image in ember's state directory by copying the .img and
/// writing registry.json. Uses the `library-{name}-{tag}` naming convention.
fn register_test_image(state_dir: &Path, name: &str, tag: &str, img_path: &Path) {
    let images_dir = state_dir.join("images").join("data");
    let local_name = format!("library-{name}-{tag}");
    let dest = images_dir.join(format!("{local_name}.img"));

    std::fs::copy(img_path, &dest)
        .unwrap_or_else(|e| panic!("failed to copy image to {}: {e}", dest.display()));

    let registry_path = state_dir.join("images").join("registry.json");
    let size = std::fs::metadata(&dest).unwrap().len();
    let registry = serde_json::json!({
        "images": [{
            "reference": format!("docker.io/library/{name}:{tag}"),
            "local_name": local_name,
            "zvol": dest.to_string_lossy(),
            "size_mib": size / (1024 * 1024),
            "pulled_at": "2024-01-01T00:00:00Z"
        }]
    });
    std::fs::write(
        &registry_path,
        serde_json::to_string_pretty(&registry).unwrap(),
    )
    .unwrap();
}

/// Create a VM by manually setting up the storage layout and state files.
///
/// This bypasses `ember vm create` and instead creates the VM directory,
/// APFS-clones the rootfs, and writes minimal vm.json metadata. This is
/// sufficient for testing APFS clone efficiency, storage-efficiency command,
/// and vm delete operations.
fn create_test_vm_manual(state_dir: &Path, vm_name: &str, image_name: &str) {
    let images_dir = state_dir.join("images").join("data");
    let local_name = format!("library-{image_name}");
    let src_img = images_dir.join(format!("{local_name}.img"));

    let vm_dir = state_dir.join("vms").join(vm_name);
    std::fs::create_dir_all(vm_dir.join("snapshots")).unwrap();

    // APFS clone the base image → VM rootfs.
    let rootfs = vm_dir.join("rootfs.img");
    let status = Command::new("cp")
        .arg("-c")
        .arg(&src_img)
        .arg(&rootfs)
        .status()
        .expect("failed to run cp -c");
    assert!(
        status.success(),
        "cp -c clone failed — are source and destination on the same APFS volume?"
    );

    // Write minimal VM metadata (vm.json).
    let metadata = serde_json::json!({
        "name": vm_name,
        "id": "00000000-0000-0000-0000-000000000000",
        "status": "created",
        "image": format!("docker.io/library/{image_name}"),
        "cpus": 1,
        "memory_mib": 256,
        "disk_size_gib": 1,
        "kernel_path": "/dev/null",
        "disk_path": rootfs.to_string_lossy(),
        "api_socket": vm_dir.join("ember-vz.sock").to_string_lossy(),
        "created_at": "2024-01-01T00:00:00Z",
        "ssh": { "user": "root", "key": "/dev/null" }
    });
    std::fs::write(
        vm_dir.join("vm.json"),
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .unwrap();
}

/// Set up ember init, create a test image, register it, and create a VM.
fn setup_with_vm(tmp: &Path, test_name: &str, vm_name: &str) -> PathBuf {
    let state_dir = common::macos::setup_init(tmp);
    let img = create_test_image(tmp, test_name, 64);
    register_test_image(&state_dir, "testimg", "latest", &img);
    create_test_vm_manual(&state_dir, vm_name, "testimg-latest");
    state_dir
}

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
    let img = create_test_image(tmp.path(), "cowtest", 64);
    register_test_image(&state_dir, "cowimg", "latest", &img);

    // Measure free space before cloning.
    let free_before = get_free_space_bytes(tmp.path());

    // Create multiple VMs (each is a cp -c clone).
    for i in 0..5 {
        let vm_name = format!("cowvm{i}");
        create_test_vm_manual(&state_dir, &vm_name, "cowimg-latest");
    }

    // Measure free space after cloning.
    let free_after = get_free_space_bytes(tmp.path());

    // CoW clones should use negligible extra space. Allow up to 5MB total
    // for metadata overhead (5 clones of 64MB = 320MB logical, ~0MB actual).
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
    let img = create_test_image(tmp.path(), "efftest", 64);
    register_test_image(&state_dir, "effimg", "latest", &img);

    // Create VMs and snapshots.
    for i in 0..3 {
        let vm_name = format!("effvm{i}");
        create_test_vm_manual(&state_dir, &vm_name, "effimg-latest");

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
    let state_dir = setup_with_vm(tmp.path(), "deltest", "delvm");
    let state = state_dir.to_str().unwrap();

    // Create a snapshot too.
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

    // Create a 64MB HFS+ disk image.
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

    // Mount the HFS+ volume.
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

    // Parse mount point from plist output.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mount_point = parse_hdiutil_mount_point(&stdout)
        .unwrap_or_else(|| panic!("no mount point found in hdiutil output:\n{stdout}"));

    // RAII guard: ensure we always detach the volume on test exit.
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

    // Set up ember state directory on the HFS+ volume.
    let state_dir = PathBuf::from(&mount_point).join("ember-state");

    // `ember init` should succeed but warn about non-APFS.
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

    // Verify that `cp -c` between the APFS boot volume and the HFS+ volume
    // fails with a clear cross-device error.
    let img = create_test_image(tmp.path(), "hfstest", 8);
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse mount point from `hdiutil attach -plist` XML output.
fn parse_hdiutil_mount_point(plist_xml: &str) -> Option<String> {
    let marker = "<key>mount-point</key>";
    let idx = plist_xml.find(marker)?;
    let after = &plist_xml[idx + marker.len()..];
    let start_tag = "<string>";
    let end_tag = "</string>";
    let s_start = after.find(start_tag)? + start_tag.len();
    let s_end = after.find(end_tag)?;
    if s_start <= s_end {
        Some(after[s_start..s_end].to_string())
    } else {
        None
    }
}

/// Get free space in bytes for the volume containing the given path.
fn get_free_space_bytes(path: &Path) -> u64 {
    let output = Command::new("df")
        .arg("-k")
        .arg(path)
        .output()
        .expect("failed to run df");
    assert!(output.status.success(), "df failed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().nth(1).expect("df output too short");
    let fields: Vec<&str> = line.split_whitespace().collect();
    let avail_kb: u64 = fields[3].parse().expect("failed to parse df available");
    avail_kb * 1024
}
