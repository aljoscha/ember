//! Integration tests for `ember snapshot create`, `snapshot list`,
//! `snapshot restore`, and `snapshot delete` (Linux-only).
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
//!   ./run-integration-tests.sh snapshot

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Basic snapshot lifecycle: create → list → delete.
#[test]
#[ignore]
fn snapshot_create_list_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("snapbasic", "snapvm1", &tmp);
    let state = state_dir.to_str().unwrap();
    let vm_zvol = format!("{pool}/ember/vms/snapvm1");

    // -- Create snapshot --
    let output = common::ember(&[
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
    common::linux::assert_snapshot_exists(&format!("{vm_zvol}@snap1"));

    // -- Create a second snapshot --
    let output = common::ember(&[
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
    common::linux::assert_snapshot_exists(&format!("{vm_zvol}@snap2"));

    // -- List snapshots --
    let output = common::ember(&["--state-dir", state, "snapshot", "list", "snapvm1"]);
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
    let output = common::ember(&[
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
    let output = common::ember(&[
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
    common::linux::assert_snapshot_absent(&format!("{vm_zvol}@snap1"));
    // snap2 should still exist.
    common::linux::assert_snapshot_exists(&format!("{vm_zvol}@snap2"));

    // -- Delete snap2 --
    let output = common::ember(&[
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
    common::linux::assert_snapshot_absent(&format!("{vm_zvol}@snap2"));
}

/// Snapshot → modify zvol → restore → verify original state.
///
/// This is the core data-integrity test: it proves that ZFS rollback
/// actually reverts the VM's disk contents to the snapshot point.
#[test]
#[ignore]
fn snapshot_restore_reverts_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("snaprestore", "restorevm", &tmp);
    let state = state_dir.to_str().unwrap();
    let vm_zvol = format!("{pool}/ember/vms/restorevm");
    let zvol_device = format!("/dev/zvol/{vm_zvol}");

    assert!(
        common::linux::wait_for_zvol_device(&zvol_device),
        "zvol device {zvol_device} did not appear within timeout"
    );

    // Write a marker file to the zvol BEFORE snapshotting.
    common::linux::with_mounted_zvol(&zvol_device, |mount| {
        std::fs::write(mount.join("before-snapshot.txt"), "original-content\n").unwrap();
    });

    // -- Create snapshot --
    let output = common::ember(&[
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
    common::linux::assert_snapshot_exists(&format!("{vm_zvol}@checkpoint"));

    // Modify the zvol AFTER snapshotting: add a new file and change the
    // existing one.
    common::linux::with_mounted_zvol(&zvol_device, |mount| {
        std::fs::write(mount.join("after-snapshot.txt"), "this should disappear\n").unwrap();
        std::fs::write(mount.join("before-snapshot.txt"), "modified-content\n").unwrap();
    });

    // Verify the modifications are present before restore.
    common::linux::with_mounted_zvol(&zvol_device, |mount| {
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
    let output = common::ember(&[
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
    common::linux::with_mounted_zvol(&zvol_device, |mount| {
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
    let (_pool, state_dir, _cleanup) = common::linux::setup_pool_and_vm("snapdup", "dupvm", &tmp);
    let state = state_dir.to_str().unwrap();

    // Create first snapshot.
    let output = common::ember(&[
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
    let output = common::ember(&[
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
    let (_pool, state_dir, _cleanup) = common::linux::setup_pool_and_vm("snapbase", "basevm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&["--state-dir", state, "snapshot", "create", "basevm", "base"]);
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
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("snapdelbase", "delbasevm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&[
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
    let pool = common::linux::test_pool("snapnovm");
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = common::linux::create_loop_device(tmp.path());

    let _cleanup = common::linux::PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    // Just init, no VM created.
    let output = common::ember(&[
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
    let output = common::ember(&[
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
    let output = common::ember(&["--state-dir", state, "snapshot", "list", "nosuchvm"]);
    assert!(
        !output.status.success(),
        "expected snapshot list on non-existent VM to fail"
    );

    // snapshot restore on non-existent VM.
    let output = common::ember(&[
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
    let output = common::ember(&[
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
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("snaprestorenosnap", "novm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&[
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
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("snapdelnosnap", "delnovm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&[
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
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("snapempty", "emptyvm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&["--state-dir", state, "snapshot", "list", "emptyvm"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    // With no user snapshots (@base is hidden), should indicate no snapshots.
    assert!(
        stdout.contains("No snapshots") || stdout.contains("no snapshots"),
        "expected empty snapshot message: {stdout}"
    );
}
