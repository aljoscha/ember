//! Integration tests for `ember vm fork` (Linux-only).
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
//!   ./run-integration-tests.sh fork

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Basic fork lifecycle: fork a stopped VM, verify ZFS state, inspect metadata.
#[test]
#[ignore]
fn fork_basic() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_and_vm("forkbasic", "source", &tmp);
    let state = state_dir.to_str().unwrap();

    let source_zvol = format!("{pool}/ember/vms/source");
    let forked_zvol = format!("{pool}/ember/vms/child");

    // Fork the source VM.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "fork",
        "source",
        "child",
        "--no-start",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm fork failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // The forked VM's zvol should exist.
    common::linux::assert_zvol_exists(&forked_zvol);

    // The fork snapshot should exist on the source.
    common::linux::assert_snapshot_exists(&format!("{source_zvol}@fork-child"));

    // Inspect the forked VM — verify it inherited source's image and has forked_from set.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "child",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let meta: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {stdout}"));

    assert_eq!(meta["image"], "docker.io/library/alpine:latest");
    assert_eq!(meta["status"], "created");
    assert_eq!(meta["forked_from"], format!("{source_zvol}@fork-child"),);

    eprintln!("Basic fork verified.");
}

/// Fork with CLI overrides for cpus and memory.
#[test]
#[ignore]
fn fork_with_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("forkoverride", "src", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "fork",
        "src",
        "dst",
        "--cpus",
        "4",
        "--memory",
        "1G",
        "--no-start",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm fork failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Inspect and verify overrides took effect.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "dst",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    let meta: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {stdout}"));

    assert_eq!(meta["cpus"], 4, "cpus override not applied");
    assert_eq!(meta["memory_mib"], 1024, "memory override not applied");

    eprintln!("Fork with overrides verified.");
}

/// Deleting a forked VM cleans up the fork snapshot on the source.
#[test]
#[ignore]
fn fork_delete_cleans_up_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_and_vm("forkdel", "base", &tmp);
    let state = state_dir.to_str().unwrap();

    let base_zvol = format!("{pool}/ember/vms/base");
    let forked_zvol = format!("{pool}/ember/vms/forked");

    // Fork the VM.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "fork",
        "base",
        "forked",
        "--no-start",
    ]);
    assert!(
        output.status.success(),
        "vm fork failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify fork snapshot exists on source.
    common::linux::assert_snapshot_exists(&format!("{base_zvol}@fork-forked"));
    common::linux::assert_zvol_exists(&forked_zvol);

    // Delete the forked VM.
    let output = common::ember(&["--state-dir", state, "vm", "delete", "forked"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm delete failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // The forked VM's zvol should be gone.
    common::linux::assert_zvol_absent(&forked_zvol);

    // The fork snapshot on the source should also be cleaned up.
    common::linux::assert_snapshot_absent(&format!("{base_zvol}@fork-forked"));

    // The source VM should still exist and be intact.
    common::linux::assert_zvol_exists(&base_zvol);

    eprintln!("Fork snapshot cleanup on delete verified.");
}

/// Deleting the source VM still works even when a fork snapshot exists
/// (because source destroy is recursive).
#[test]
#[ignore]
fn fork_delete_source_with_dependent_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("forksrcdel", "origin", &tmp);
    let state = state_dir.to_str().unwrap();

    let origin_zvol = format!("{pool}/ember/vms/origin");

    // Fork the VM.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "fork",
        "origin",
        "clone1",
        "--no-start",
    ]);
    assert!(
        output.status.success(),
        "vm fork failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Delete the forked VM first (to remove the clone dependency).
    let output = common::ember(&["--state-dir", state, "vm", "delete", "clone1"]);
    assert!(
        output.status.success(),
        "vm delete clone1 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Now delete the source VM — should succeed since fork snapshot was cleaned.
    let output = common::ember(&["--state-dir", state, "vm", "delete", "origin"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm delete origin failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    common::linux::assert_zvol_absent(&origin_zvol);

    eprintln!("Source deletion after fork cleanup verified.");
}

/// Forking a non-existent VM should fail.
#[test]
#[ignore]
fn fork_nonexistent_source_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("forknoexist", "realvm", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "fork",
        "nosuchvm",
        "child",
        "--no-start",
    ]);
    assert!(
        !output.status.success(),
        "expected fork of non-existent VM to fail"
    );
}

/// Forking into an existing VM name should fail.
#[test]
#[ignore]
fn fork_duplicate_name_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("forkdup", "existing", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "fork",
        "existing",
        "existing",
        "--no-start",
    ]);
    assert!(
        !output.status.success(),
        "expected fork with duplicate name to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' error: {stderr}"
    );
}

/// Forking with --disk-size smaller than source should fail.
#[test]
#[ignore]
fn fork_shrink_disk_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("forkshrink", "bigvm", &tmp);
    let state = state_dir.to_str().unwrap();

    // Default disk size is 8G, try to shrink to 1G.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "fork",
        "bigvm",
        "smallvm",
        "--disk-size",
        "1G",
        "--no-start",
    ]);
    assert!(
        !output.status.success(),
        "expected fork with smaller disk to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot shrink") || stderr.contains("shrink"),
        "expected shrink error: {stderr}"
    );
}

/// Fork preserves data from the source VM's disk.
#[test]
#[ignore]
fn fork_preserves_disk_data() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_and_vm("forkdata", "datavm", &tmp);
    let state = state_dir.to_str().unwrap();

    let source_zvol = format!("{pool}/ember/vms/datavm");
    let forked_zvol = format!("{pool}/ember/vms/datachild");
    let source_device = format!("/dev/zvol/{source_zvol}");
    let forked_device = format!("/dev/zvol/{forked_zvol}");

    // Wait for the source zvol device.
    for _ in 0..50 {
        if std::path::Path::new(&source_device).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        std::path::Path::new(&source_device).exists(),
        "source zvol device did not appear"
    );

    // Write a marker file to the source VM's disk.
    let mount_dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("mount")
        .arg(&source_device)
        .arg(mount_dir.path())
        .status()
        .expect("mount failed");
    assert!(status.success(), "failed to mount source zvol");

    std::fs::write(
        mount_dir.path().join("fork-test-marker.txt"),
        "hello from source\n",
    )
    .unwrap();

    let status = std::process::Command::new("umount")
        .arg(mount_dir.path())
        .status()
        .expect("umount failed");
    assert!(status.success(), "failed to umount source zvol");

    // Fork the VM.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "fork",
        "datavm",
        "datachild",
        "--no-start",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm fork failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Wait for the forked zvol device.
    for _ in 0..50 {
        if std::path::Path::new(&forked_device).exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(
        std::path::Path::new(&forked_device).exists(),
        "forked zvol device did not appear"
    );

    // Mount the forked VM's disk and verify the marker file is there.
    let fork_mount = tempfile::tempdir().unwrap();
    let status = std::process::Command::new("mount")
        .arg(&forked_device)
        .arg(fork_mount.path())
        .status()
        .expect("mount failed");
    assert!(status.success(), "failed to mount forked zvol");

    let content = std::fs::read_to_string(fork_mount.path().join("fork-test-marker.txt")).unwrap();
    assert_eq!(
        content, "hello from source\n",
        "forked VM should have data from source"
    );

    let status = std::process::Command::new("umount")
        .arg(fork_mount.path())
        .status()
        .expect("umount failed");
    assert!(status.success(), "failed to umount forked zvol");

    eprintln!("Fork data preservation verified.");
}
