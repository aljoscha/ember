//! Integration tests for macOS APFS storage backend.
//!
//! These tests verify:
//! - APFS CoW clone operations (cp -c for VM cloning, snapshots)
//! - Snapshot create/list/restore/delete via CLI
//! - APFS clone space efficiency (CoW doesn't consume extra space)
//! - Storage efficiency debug command
//! - VM delete removes storage
//!
//! Requirements:
//! - macOS with APFS filesystem (default since 10.13)
//! - Homebrew `e2fsprogs` (for mkfs.ext4)
//! - No root required
//!
//! Note: These tests bypass `ember vm create` (which requires ext4 mount
//! support not yet implemented on macOS) and instead set up VM state manually
//! using direct file operations + `cp -c`. This tests the storage backend
//! and snapshot CLI without the full VM creation pipeline.
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
/// This bypasses `ember vm create` (which requires ext4 mount support) and
/// instead creates the VM directory, APFS-clones the rootfs, and writes
/// minimal vm.json metadata. This is sufficient for testing snapshot, delete,
/// and storage efficiency operations.
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
// Phase 2: Storage Backend Tests
// ---------------------------------------------------------------------------

/// Full snapshot lifecycle: create → list → restore → delete.
#[test]
#[ignore]
fn storage_lifecycle_create_clone_snapshot_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_with_vm(tmp.path(), "lifecycle", "testvm");
    let state = state_dir.to_str().unwrap();

    // Verify VM rootfs exists.
    let rootfs = state_dir.join("vms").join("testvm").join("rootfs.img");
    assert!(rootfs.exists(), "rootfs.img should exist after setup");

    // --- Create snapshot ---
    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "testvm",
        "snap1",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "snapshot create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    let snap_file = state_dir
        .join("vms")
        .join("testvm")
        .join("snapshots")
        .join("snap1.img");
    assert!(snap_file.exists(), "snapshot file should exist");

    // --- Create second snapshot ---
    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "testvm",
        "snap2",
    ]);
    assert!(
        output.status.success(),
        "snapshot create snap2 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // --- List snapshots ---
    let output = common::ember(&["--state-dir", state, "snapshot", "list", "testvm"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(stdout.contains("snap1"), "expected 'snap1' in: {stdout}");
    assert!(stdout.contains("snap2"), "expected 'snap2' in: {stdout}");

    // --- JSON list ---
    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "list",
        "testvm",
        "--format",
        "json",
    ]);
    let json_stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {json_stdout}"));
    let snapshots = parsed.as_array().expect("expected JSON array");
    assert_eq!(
        snapshots.len(),
        2,
        "expected 2 snapshots, got: {json_stdout}"
    );

    // --- Restore snapshot ---
    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "restore",
        "testvm",
        "snap1",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "snapshot restore failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(rootfs.exists(), "rootfs.img should exist after restore");

    // --- Delete snapshots ---
    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "delete",
        "testvm",
        "snap1",
    ]);
    assert!(
        output.status.success(),
        "snapshot delete failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!snap_file.exists(), "snap1.img should be gone after delete");

    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "delete",
        "testvm",
        "snap2",
    ]);
    assert!(
        output.status.success(),
        "snapshot delete snap2 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // --- Snapshot list should now be empty ---
    let output = common::ember(&["--state-dir", state, "snapshot", "list", "testvm"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(
        stdout.contains("No snapshots") || stdout.contains("no snapshots"),
        "expected empty snapshot message: {stdout}"
    );
}

/// Duplicate snapshot name should fail.
#[test]
#[ignore]
fn snapshot_create_duplicate_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_with_vm(tmp.path(), "snapdup", "dupvm");
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "dupvm",
        "mysnap",
    ]);
    assert!(output.status.success());

    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "dupvm",
        "mysnap",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' error: {stderr}"
    );
}

/// Reserved snapshot name "base" should be rejected.
#[test]
#[ignore]
fn snapshot_create_base_name_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_with_vm(tmp.path(), "snapbase", "basevm");
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&["--state-dir", state, "snapshot", "create", "basevm", "base"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("reserved") || stderr.contains("base"),
        "expected error about reserved name: {stderr}"
    );
}

/// Restoring a non-existent snapshot should fail.
#[test]
#[ignore]
fn snapshot_restore_nonexistent_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_with_vm(tmp.path(), "snaprestnosnap", "novm");
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "restore",
        "novm",
        "nosuchsnap",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("does not exist"),
        "expected error about missing snapshot: {stderr}"
    );
}

/// Deleting a non-existent snapshot should fail.
#[test]
#[ignore]
fn snapshot_delete_nonexistent_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_with_vm(tmp.path(), "snapdelnosnap", "delnovm");
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&[
        "--state-dir",
        state,
        "snapshot",
        "delete",
        "delnovm",
        "nosuchsnap",
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("does not exist"),
        "expected error about missing snapshot: {stderr}"
    );
}

/// Snapshot list on a VM with no snapshots should show empty result.
#[test]
#[ignore]
fn snapshot_list_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_with_vm(tmp.path(), "snapempty", "emptyvm");
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&["--state-dir", state, "snapshot", "list", "emptyvm"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(
        stdout.contains("No snapshots") || stdout.contains("no snapshots"),
        "expected empty snapshot message: {stdout}"
    );
}

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
// Phase 2: Resize tests
// ---------------------------------------------------------------------------

/// Resize a stopped VM: verify .img file grows, ext4 expands, metadata updates.
#[test]
#[ignore]
fn resize_grows_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_with_vm(tmp.path(), "resize", "resizevm");
    let state = state_dir.to_str().unwrap();
    let rootfs = state_dir.join("vms").join("resizevm").join("rootfs.img");

    // Initial file size should be 64MB (from create_test_image).
    let initial_size = std::fs::metadata(&rootfs).unwrap().len();
    assert_eq!(
        initial_size,
        64 * 1024 * 1024,
        "initial image should be 64MB"
    );

    // --- Resize to 2 GiB ---
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "resizevm",
        "--disk-size",
        "2G",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm resize failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("resized"),
        "expected confirmation message: {stdout}"
    );

    // --- Verify file size grew to 2 GiB ---
    let new_size = std::fs::metadata(&rootfs).unwrap().len();
    assert_eq!(
        new_size,
        2 * 1024 * 1024 * 1024,
        "rootfs.img should be 2 GiB after resize, got {new_size} bytes"
    );

    // --- Verify metadata updated ---
    let inspect = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "resizevm",
        "--format",
        "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
        .expect("failed to parse inspect JSON");
    assert_eq!(
        json["disk_size_gib"], 2,
        "metadata should show 2 GiB after resize"
    );

    // --- Verify ext4 filesystem was expanded ---
    // Use dumpe2fs to check the block count reflects ~2 GiB.
    let dumpe2fs = common::macos::find_e2fsprogs_tool("dumpe2fs");
    let output = Command::new(&dumpe2fs)
        .arg("-h")
        .arg(&rootfs)
        .output()
        .unwrap_or_else(|_| panic!("failed to run {dumpe2fs} — is e2fsprogs installed?"));
    assert!(
        output.status.success(),
        "dumpe2fs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dump_stdout = String::from_utf8_lossy(&output.stdout);
    let block_count: u64 = parse_dumpe2fs_value(&dump_stdout, "Block count");
    let block_size: u64 = parse_dumpe2fs_value(&dump_stdout, "Block size");
    let fs_bytes = block_count * block_size;

    // ext4 has some overhead, so the filesystem won't be exactly 2 GiB.
    // Check it's at least 1.8 GiB (allowing ~10% for metadata overhead).
    let min_expected = (1.8 * 1024.0 * 1024.0 * 1024.0) as u64;
    assert!(
        fs_bytes >= min_expected,
        "ext4 filesystem should be ~2 GiB after resize, got {fs_bytes} bytes ({:.2} GiB)",
        fs_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    );
}

/// Shrinking (or same size) should be rejected.
#[test]
#[ignore]
fn resize_shrink_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_with_vm(tmp.path(), "resizeshrink", "shrinkvm");
    let state = state_dir.to_str().unwrap();

    // metadata has disk_size_gib: 1; try same size.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "shrinkvm",
        "--disk-size",
        "1G",
    ]);
    assert!(
        !output.status.success(),
        "expected resize to same size to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be larger"),
        "expected 'must be larger' error: {stderr}"
    );
}

/// Multiple sequential resizes should all succeed.
#[test]
#[ignore]
fn resize_multiple_grows() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = setup_with_vm(tmp.path(), "resizemulti", "multivm");
    let state = state_dir.to_str().unwrap();
    let rootfs = state_dir.join("vms").join("multivm").join("rootfs.img");

    // Resize 1 GiB → 2 GiB.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "multivm",
        "--disk-size",
        "2G",
    ]);
    assert!(
        output.status.success(),
        "first resize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::metadata(&rootfs).unwrap().len(),
        2 * 1024 * 1024 * 1024
    );

    // Resize 2 GiB → 4 GiB.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "multivm",
        "--disk-size",
        "4G",
    ]);
    assert!(
        output.status.success(),
        "second resize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::metadata(&rootfs).unwrap().len(),
        4 * 1024 * 1024 * 1024
    );

    // Verify metadata tracks the latest size.
    let inspect = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "multivm",
        "--format",
        "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
        .expect("failed to parse inspect JSON");
    assert_eq!(json["disk_size_gib"], 4, "metadata should show 4 GiB");
}

// ---------------------------------------------------------------------------
// Phase 2: Non-APFS failure tests
// ---------------------------------------------------------------------------

/// Verify that ember warns about non-APFS volumes and detects missing CoW.
///
/// On macOS, `cp -c` silently falls back to a full copy on non-APFS
/// filesystems (it only fails for cross-device clones). Ember handles
/// this with two detection mechanisms:
///
/// 1. **`ember init`** checks the state directory's filesystem type via
///    `diskutil info` and warns if it's not APFS.
/// 2. **Timing check**: the `apfs_clone()` helper warns if a clone takes
///    over 1 second (indicating a full copy instead of instant CoW).
///
/// This test creates an HFS+ disk image, mounts it, and verifies that
/// `ember init` produces the appropriate warning about non-APFS storage.
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
    // The warning is printed to stderr by check_apfs_volume().
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
    // fails with a clear cross-device error. This is the case where `cp -c`
    // actually errors (same-volume non-APFS silently falls back to full copy).
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
/// Looks for the `<key>mount-point</key>` entry and returns the value.
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

/// Parse a numeric value from `dumpe2fs -h` output.
///
/// Looks for a line like `Block count:      524288` and returns the number.
fn parse_dumpe2fs_value(output: &str, key: &str) -> u64 {
    let prefix = format!("{key}:");
    let line = output
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("expected '{prefix}' in dumpe2fs output"));
    line.split(':')
        .nth(1)
        .unwrap()
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("failed to parse '{key}' value: {e}\nline: {line}"))
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
    // Field 3 is "Available" in 1024-byte blocks.
    let avail_kb: u64 = fields[3].parse().expect("failed to parse df available");
    avail_kb * 1024
}
