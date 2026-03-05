//! Integration tests for `ember snapshot create`, `snapshot list`,
//! `snapshot restore`, and `snapshot delete`.
//!
//! These tests require:
//! - Root privileges
//! - Working ZFS installation
//! - Network access (to pull OCI images from Docker Hub)
//! - `skopeo` installed
//!
//! They are marked `#[ignore]` so `cargo test` skips them by default.
//!
//! To run:
//!   ./run-integration-tests.sh snapshot

use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// Shared helpers (same pattern as init.rs, image.rs, vm.rs)
// ---------------------------------------------------------------------------

fn test_pool(name: &str) -> String {
    format!("embertest_{name}_{}", std::process::id())
}

fn create_loop_device(dir: &std::path::Path) -> (String, PathBuf) {
    let file = dir.join("pool.img");

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

fn detach_loop_device(dev: &str) {
    let _ = Command::new("losetup").args(["-d", dev]).status();
}

fn destroy_pool(pool: &str) {
    let _ = Command::new("zpool").args(["destroy", "-f", pool]).status();
}

fn ember_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("ember");
    path
}

fn ember(args: &[&str]) -> std::process::Output {
    Command::new(ember_bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute ember: {e}"))
}

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

fn assert_snapshot_absent(snapshot: &str) {
    let output = Command::new("zfs")
        .args(["list", "-t", "snapshot", "-H", snapshot])
        .output()
        .expect("failed to run zfs");
    assert!(
        !output.status.success(),
        "expected snapshot '{snapshot}' to NOT exist"
    );
}

/// Set up a ZFS pool, run `ember init`, pull alpine, and create a VM.
/// Returns (pool_name, state_dir, cleanup_guard).
fn setup_pool_and_vm(
    test_name: &str,
    vm_name: &str,
    tmp: &tempfile::TempDir,
) -> (String, PathBuf, PoolCleanup) {
    let pool = test_pool(test_name);
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = create_loop_device(tmp.path());

    let cleanup = PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    // Init pool.
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

    // Pull alpine image.
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

    // Create a dummy kernel (no actual booting needed).
    let kernel = tmp.path().join("vmlinux-dummy");
    std::fs::write(&kernel, b"not a real kernel").unwrap();

    // Create a VM (--no-start, no Firecracker needed).
    let output = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "vm",
        "create",
        vm_name,
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--no-start",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    (pool, state_dir, cleanup)
}

/// Wait for a ZFS zvol device node to appear, up to ~5 seconds.
fn wait_for_zvol_device(device_path: &str) -> bool {
    for _ in 0..50 {
        if std::path::Path::new(device_path).exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Mount a zvol, run a closure with the mount path, then unmount.
/// Returns the closure's result.
fn with_mounted_zvol<F, T>(zvol_device: &str, f: F) -> T
where
    F: FnOnce(&std::path::Path) -> T,
{
    let mount_dir = tempfile::tempdir().unwrap();
    let mount_path = mount_dir.path();

    let status = Command::new("mount")
        .args(["-o", "rw"])
        .arg(zvol_device)
        .arg(mount_path)
        .status()
        .expect("failed to run mount");
    assert!(
        status.success(),
        "failed to mount {zvol_device} at {}",
        mount_path.display()
    );

    let result = f(mount_path);

    let status = Command::new("umount")
        .arg(mount_path)
        .status()
        .expect("failed to run umount");
    assert!(
        status.success(),
        "failed to unmount {}",
        mount_path.display()
    );

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Basic snapshot lifecycle: create → list → delete.
#[test]
#[ignore]
fn snapshot_create_list_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = setup_pool_and_vm("snapbasic", "snapvm1", &tmp);
    let state = state_dir.to_str().unwrap();
    let vm_zvol = format!("{pool}/ember/vms/snapvm1");

    // -- Create snapshot --
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "snapvm1",
        "snap1",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "snapshot create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_snapshot_exists(&format!("{vm_zvol}@snap1"));

    // -- Create a second snapshot --
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "snapvm1",
        "snap2",
    ]);
    assert!(
        output.status.success(),
        "snapshot create snap2 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_snapshot_exists(&format!("{vm_zvol}@snap2"));

    // -- List snapshots --
    let output = ember(&["--state-dir", state, "snapshot", "list", "snapvm1"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(
        stdout.contains("snap1"),
        "expected 'snap1' in snapshot list: {stdout}"
    );
    assert!(
        stdout.contains("snap2"),
        "expected 'snap2' in snapshot list: {stdout}"
    );
    // The internal @base snapshot should NOT appear in user-facing list.
    assert!(
        !stdout.contains("base"),
        "internal @base snapshot should be hidden from list: {stdout}"
    );

    // -- JSON list --
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "list",
        "snapvm1",
        "--format",
        "json",
    ]);
    let json_stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {json_stdout}"));
    let snapshots = parsed.as_array().expect("expected JSON array of snapshots");
    assert_eq!(
        snapshots.len(),
        2,
        "expected 2 snapshots in JSON list, got: {json_stdout}"
    );

    // -- Delete snap1 --
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "delete",
        "snapvm1",
        "snap1",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "snapshot delete failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_snapshot_absent(&format!("{vm_zvol}@snap1"));
    // snap2 should still exist.
    assert_snapshot_exists(&format!("{vm_zvol}@snap2"));

    // -- Delete snap2 --
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "delete",
        "snapvm1",
        "snap2",
    ]);
    assert!(
        output.status.success(),
        "snapshot delete snap2 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_snapshot_absent(&format!("{vm_zvol}@snap2"));
}

/// Snapshot → modify zvol → restore → verify original state.
///
/// This is the core data-integrity test: it proves that ZFS rollback
/// actually reverts the VM's disk contents to the snapshot point.
#[test]
#[ignore]
fn snapshot_restore_reverts_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = setup_pool_and_vm("snaprestore", "restorevm", &tmp);
    let state = state_dir.to_str().unwrap();
    let vm_zvol = format!("{pool}/ember/vms/restorevm");
    let zvol_device = format!("/dev/zvol/{vm_zvol}");

    assert!(
        wait_for_zvol_device(&zvol_device),
        "zvol device {zvol_device} did not appear within timeout"
    );

    // Write a marker file to the zvol BEFORE snapshotting.
    with_mounted_zvol(&zvol_device, |mount| {
        std::fs::write(mount.join("before-snapshot.txt"), "original-content\n").unwrap();
    });

    // -- Create snapshot --
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "restorevm",
        "checkpoint",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "snapshot create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert_snapshot_exists(&format!("{vm_zvol}@checkpoint"));

    // Modify the zvol AFTER snapshotting: add a new file and change the
    // existing one.
    with_mounted_zvol(&zvol_device, |mount| {
        std::fs::write(mount.join("after-snapshot.txt"), "this should disappear\n").unwrap();
        std::fs::write(mount.join("before-snapshot.txt"), "modified-content\n").unwrap();
    });

    // Verify the modifications are present before restore.
    with_mounted_zvol(&zvol_device, |mount| {
        assert!(
            mount.join("after-snapshot.txt").exists(),
            "after-snapshot.txt should exist before restore"
        );
        let content = std::fs::read_to_string(mount.join("before-snapshot.txt")).unwrap();
        assert_eq!(
            content, "modified-content\n",
            "before-snapshot.txt should have modified content before restore"
        );
    });

    // -- Restore snapshot --
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "restore",
        "restorevm",
        "checkpoint",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "snapshot restore failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // -- Verify original state is restored --
    with_mounted_zvol(&zvol_device, |mount| {
        // The file added after the snapshot should be gone.
        assert!(
            !mount.join("after-snapshot.txt").exists(),
            "after-snapshot.txt should NOT exist after restore"
        );

        // The file from before the snapshot should have its original content.
        let content = std::fs::read_to_string(mount.join("before-snapshot.txt")).unwrap();
        assert_eq!(
            content, "original-content\n",
            "before-snapshot.txt should be reverted to original content"
        );
    });

    eprintln!("Snapshot restore data-integrity verified.");
}

/// Duplicate snapshot name should fail.
#[test]
#[ignore]
fn snapshot_create_duplicate_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_and_vm("snapdup", "dupvm", &tmp);
    let state = state_dir.to_str().unwrap();

    // Create first snapshot.
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "dupvm",
        "mysnap",
    ]);
    assert!(
        output.status.success(),
        "first snapshot create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Creating with the same name should fail.
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "dupvm",
        "mysnap",
    ]);
    assert!(
        !output.status.success(),
        "expected duplicate snapshot create to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' error: {stderr}"
    );
}

/// Cannot create a snapshot named "base" (reserved for internal use).
#[test]
#[ignore]
fn snapshot_create_base_name_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_and_vm("snapbase", "basevm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = ember(&["--state-dir", state, "snapshot", "create", "basevm", "base"]);
    assert!(
        !output.status.success(),
        "expected snapshot create 'base' to be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("reserved") || stderr.contains("base"),
        "expected error about reserved name: {stderr}"
    );
}

/// Cannot delete the internal @base snapshot.
#[test]
#[ignore]
fn snapshot_delete_base_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_and_vm("snapdelbase", "delbasevm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "delete",
        "delbasevm",
        "base",
    ]);
    assert!(
        !output.status.success(),
        "expected snapshot delete 'base' to be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("reserved") || stderr.contains("base"),
        "expected error about reserved name: {stderr}"
    );
}

/// Snapshot operations on a non-existent VM should fail.
#[test]
#[ignore]
fn snapshot_on_nonexistent_vm_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = test_pool("snapnovm");
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = create_loop_device(tmp.path());

    let _cleanup = PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    // Just init, no VM created.
    let output = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--pool",
        &pool,
        "--device",
        &loop_dev,
    ]);
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let state = state_dir.to_str().unwrap();

    // snapshot create on non-existent VM.
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "create",
        "nosuchvm",
        "snap1",
    ]);
    assert!(
        !output.status.success(),
        "expected snapshot create on non-existent VM to fail"
    );

    // snapshot list on non-existent VM.
    let output = ember(&["--state-dir", state, "snapshot", "list", "nosuchvm"]);
    assert!(
        !output.status.success(),
        "expected snapshot list on non-existent VM to fail"
    );

    // snapshot restore on non-existent VM.
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "restore",
        "nosuchvm",
        "snap1",
    ]);
    assert!(
        !output.status.success(),
        "expected snapshot restore on non-existent VM to fail"
    );

    // snapshot delete on non-existent VM.
    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "delete",
        "nosuchvm",
        "snap1",
    ]);
    assert!(
        !output.status.success(),
        "expected snapshot delete on non-existent VM to fail"
    );
}

/// Restoring a non-existent snapshot should fail.
#[test]
#[ignore]
fn snapshot_restore_nonexistent_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_and_vm("snaprestorenosnap", "novm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "restore",
        "novm",
        "nosuchsnap",
    ]);
    assert!(
        !output.status.success(),
        "expected restore of non-existent snapshot to fail"
    );
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
    let (_pool, state_dir, _cleanup) = setup_pool_and_vm("snapdelnosnap", "delnovm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = ember(&[
        "--state-dir",
        state,
        "snapshot",
        "delete",
        "delnovm",
        "nosuchsnap",
    ]);
    assert!(
        !output.status.success(),
        "expected delete of non-existent snapshot to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("does not exist"),
        "expected error about missing snapshot: {stderr}"
    );
}

/// Snapshot list on a VM with no snapshots should show an empty result.
#[test]
#[ignore]
fn snapshot_list_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_and_vm("snapempty", "emptyvm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = ember(&["--state-dir", state, "snapshot", "list", "emptyvm"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    // With no user snapshots (@base is hidden), should indicate no snapshots.
    assert!(
        stdout.contains("No snapshots") || stdout.contains("no snapshots"),
        "expected empty snapshot message: {stdout}"
    );
}
