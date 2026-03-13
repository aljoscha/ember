//! Integration tests for `ember vm fork`.
//!
//! Cross-platform tests use `TestEnv::with_vm()` to abstract platform setup.
//! All shared tests use `--no-start` so no hypervisor is needed.
//! Platform-specific storage verification (ZFS snapshot cleanup on Linux)
//! is gated with `#[cfg(target_os)]`.
//!
//! To run:
//!   ./run-integration-tests.sh fork

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Cross-platform tests
// ---------------------------------------------------------------------------

/// Basic fork: fork a stopped VM, verify metadata.
#[test]
#[ignore]
fn fork_basic() {
    let env = common::TestEnv::with_vm("forkbasic", "source");
    let state = env.state();

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

    // Inspect the forked VM.
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
    assert!(
        !meta["forked_from"].is_null(),
        "expected forked_from to be set"
    );

    // Platform-specific storage verification.
    #[cfg(target_os = "linux")]
    {
        let source_zvol = format!("{}/ember/vms/source", env.pool);
        let forked_zvol = format!("{}/ember/vms/child", env.pool);
        common::linux::assert_zvol_exists(&forked_zvol);
        common::linux::assert_snapshot_exists(&format!("{source_zvol}@fork-child"));
    }
}

/// Fork with CLI overrides for cpus and memory.
#[test]
#[ignore]
fn fork_with_overrides() {
    let env = common::TestEnv::with_vm("forkoverride", "src");
    let state = env.state();

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
}

/// Forking a non-existent VM should fail.
#[test]
#[ignore]
fn fork_nonexistent_source_fails() {
    let env = common::TestEnv::with_vm("forknoexist", "realvm");
    let state = env.state();

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
    let env = common::TestEnv::with_vm("forkdup", "existing");
    let state = env.state();

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
    let env = common::TestEnv::with_vm("forkshrink", "bigvm");
    let state = env.state();

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

// ---------------------------------------------------------------------------
// Linux-specific tests
// ---------------------------------------------------------------------------

/// Deleting a forked VM cleans up the fork snapshot on the source.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn fork_delete_cleans_up_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_and_vm("forkdel", "base", &tmp);
    let state = state_dir.to_str().unwrap();

    let base_zvol = format!("{pool}/ember/vms/base");
    let forked_zvol = format!("{pool}/ember/vms/forked");

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

    common::linux::assert_snapshot_exists(&format!("{base_zvol}@fork-forked"));
    common::linux::assert_zvol_exists(&forked_zvol);

    let output = common::ember(&["--state-dir", state, "vm", "delete", "forked"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm delete failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    common::linux::assert_zvol_absent(&forked_zvol);
    common::linux::assert_snapshot_absent(&format!("{base_zvol}@fork-forked"));
    common::linux::assert_zvol_exists(&base_zvol);
}

/// Deleting the source VM after the fork is cleaned up should succeed.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn fork_delete_source_with_dependent_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm("forksrcdel", "origin", &tmp);
    let state = state_dir.to_str().unwrap();

    let origin_zvol = format!("{pool}/ember/vms/origin");

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

    let output = common::ember(&["--state-dir", state, "vm", "delete", "clone1"]);
    assert!(
        output.status.success(),
        "vm delete clone1 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = common::ember(&["--state-dir", state, "vm", "delete", "origin"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm delete origin failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    common::linux::assert_zvol_absent(&origin_zvol);
}

/// Fork preserves data from the source VM's disk.
#[cfg(target_os = "linux")]
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

    assert!(
        common::linux::wait_for_zvol_device(&source_device),
        "source zvol device did not appear"
    );

    // Write a marker file to the source VM's disk.
    common::linux::with_mounted_zvol(&source_device, |mount| {
        std::fs::write(mount.join("fork-test-marker.txt"), "hello from source\n").unwrap();
    });

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

    // Verify the forked VM has the marker file.
    assert!(
        common::linux::wait_for_zvol_device(&forked_device),
        "forked zvol device did not appear"
    );

    common::linux::with_mounted_zvol(&forked_device, |mount| {
        let content = std::fs::read_to_string(mount.join("fork-test-marker.txt")).unwrap();
        assert_eq!(
            content, "hello from source\n",
            "forked VM should have data from source"
        );
    });
}
