//! Integration tests for vsock support.
//!
//! Tests verify CID allocation, UDS path creation, inspect output,
//! and end-to-end vsock connectivity on both platforms.
//!
//! Cross-platform tests (no hypervisor needed) use `TestEnv::with_vm()`.
//! Platform-specific tests require a running VM.
//!
//! To run:
//!   cargo test --test vsock -- --ignored

#[allow(dead_code)]
mod common;

use std::os::unix::net::UnixStream;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Cross-platform tests (no hypervisor needed)
// ---------------------------------------------------------------------------

/// Create a VM with --vsock, verify CID is allocated and inspect shows vsock info.
#[test]
#[ignore]
fn vsock_create_shows_in_inspect() {
    let env = common::TestEnv::with_image("vsock_inspect");
    let state = env.state();

    // Create a dummy kernel.
    let kernel = env.state_dir.parent().unwrap().join("vmlinux-dummy");
    std::fs::write(&kernel, b"not a real kernel").unwrap();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "vsockvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--vsock",
        "--no-start",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm create --vsock failed.\nstderr: {stderr}"
    );

    // Verify JSON inspect contains vsock info.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "vsockvm",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());

    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {stdout}"));

    // vsock field must be present with uds_path and guest_cid.
    let vsock = &parsed["vsock"];
    assert!(
        !vsock.is_null(),
        "expected vsock field in inspect output: {stdout}"
    );
    let uds_path = vsock["uds_path"].as_str().unwrap();
    assert!(
        uds_path.ends_with("/vsock.sock"),
        "unexpected uds_path: {uds_path}"
    );
    assert!(
        uds_path.contains("/vms/vsockvm/"),
        "uds_path should contain VM name: {uds_path}"
    );

    // CID should be >= 3 (0-2 are reserved).
    let cid = vsock["guest_cid"].as_u64().unwrap();
    assert!(cid >= 3, "guest_cid should be >= 3, got {cid}");

    // Verify table-format inspect also shows vsock.
    let output = common::ember(&["--state-dir", state, "vm", "inspect", "vsockvm"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(
        stdout.contains("Vsock:"),
        "table inspect should show Vsock section: {stdout}"
    );
    assert!(
        stdout.contains("UDS path:"),
        "table inspect should show UDS path: {stdout}"
    );
    assert!(
        stdout.contains("Guest CID:"),
        "table inspect should show Guest CID: {stdout}"
    );
}

/// Multiple VMs with --vsock get unique CIDs.
#[test]
#[ignore]
fn vsock_unique_cids() {
    let env = common::TestEnv::with_image("vsock_cids");
    let state = env.state();

    let kernel = env.state_dir.parent().unwrap().join("vmlinux-dummy");
    std::fs::write(&kernel, b"not a real kernel").unwrap();

    // Create three VMs with vsock.
    for name in &["vm1", "vm2", "vm3"] {
        let output = common::ember(&[
            "--state-dir",
            state,
            "vm",
            "create",
            name,
            "--image",
            "alpine:latest",
            "--kernel",
            kernel.to_str().unwrap(),
            "--vsock",
            "--no-start",
        ]);
        assert!(
            output.status.success(),
            "vm create {} failed: {}",
            name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Collect CIDs from all three VMs.
    let mut cids = Vec::new();
    for name in &["vm1", "vm2", "vm3"] {
        let output = common::ember(&[
            "--state-dir",
            state,
            "vm",
            "inspect",
            name,
            "--format",
            "json",
        ]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let cid = parsed["vsock"]["guest_cid"].as_u64().unwrap();
        cids.push(cid);
    }

    // All CIDs should be unique.
    assert_eq!(cids[0], 3, "first VM should get CID 3");
    assert_eq!(cids[1], 4, "second VM should get CID 4");
    assert_eq!(cids[2], 5, "third VM should get CID 5");
}

/// Deleting a VM with vsock frees its CID for reuse.
#[test]
#[ignore]
fn vsock_cid_reuse_after_delete() {
    let env = common::TestEnv::with_image("vsock_reuse");
    let state = env.state();

    let kernel = env.state_dir.parent().unwrap().join("vmlinux-dummy");
    std::fs::write(&kernel, b"not a real kernel").unwrap();

    // Create two VMs.
    for name in &["vm1", "vm2"] {
        let output = common::ember(&[
            "--state-dir",
            state,
            "vm",
            "create",
            name,
            "--image",
            "alpine:latest",
            "--kernel",
            kernel.to_str().unwrap(),
            "--vsock",
            "--no-start",
        ]);
        assert!(output.status.success());
    }

    // Delete vm1 (CID 3).
    let output = common::ember(&["--state-dir", state, "vm", "delete", "vm1"]);
    assert!(output.status.success());

    // Create vm3 — should reuse CID 3.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "vm3",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--vsock",
        "--no-start",
    ]);
    assert!(output.status.success());

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "vm3",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let cid = parsed["vsock"]["guest_cid"].as_u64().unwrap();
    assert_eq!(cid, 3, "vm3 should reuse freed CID 3, got {cid}");
}

/// VM without --vsock should have no vsock in inspect.
#[test]
#[ignore]
fn vsock_not_present_without_flag() {
    let env = common::TestEnv::with_vm("vsock_none", "plainvm");
    let state = env.state();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "plainvm",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());

    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        parsed.get("vsock").is_none() || parsed["vsock"].is_null(),
        "VM without --vsock should have no vsock field: {stdout}"
    );
}

/// `vm list` shows vsock checkmark for VMs with vsock enabled.
#[test]
#[ignore]
fn vsock_list_shows_checkmark() {
    let env = common::TestEnv::with_image("vsock_list");
    let state = env.state();

    let kernel = env.state_dir.parent().unwrap().join("vmlinux-dummy");
    std::fs::write(&kernel, b"not a real kernel").unwrap();

    // Create one VM with vsock and one without.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "with-vsock",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--vsock",
        "--no-start",
    ]);
    assert!(output.status.success());

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "no-vsock",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--no-start",
    ]);
    assert!(output.status.success());

    let output = common::ember(&["--state-dir", state, "vm", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());

    // Find the lines for each VM.
    let with_line = stdout
        .lines()
        .find(|l| l.contains("with-vsock"))
        .expect("with-vsock not in list");
    let without_line = stdout
        .lines()
        .find(|l| l.contains("no-vsock"))
        .expect("no-vsock not in list");

    assert!(
        with_line.contains('✓'),
        "with-vsock should show ✓: {with_line}"
    );
    assert!(
        !without_line.contains('✓'),
        "no-vsock should not show ✓: {without_line}"
    );
}

// ---------------------------------------------------------------------------
// macOS end-to-end test (requires ember-vz + AVF)
// ---------------------------------------------------------------------------

/// Boot a VM with --vsock, verify the UDS appears and accepts connections.
///
/// This test:
/// 1. Creates and starts a VM with --vsock
/// 2. Verifies the UDS file exists at the expected path
/// 3. Verifies a host process can connect to the UDS
/// 4. Cleans up the VM
///
/// Note: Full data exchange requires a vsock listener in the guest (emberd),
/// which is not yet implemented. This test verifies the host-side plumbing.
#[cfg(target_os = "macos")]
#[test]
#[ignore]
fn vsock_uds_accepts_connections_macos() {
    let env = common::TestEnv::with_image("vsock_e2e_macos");
    let state = env.state();

    let kernel = common::macos::ensure_kernel();

    // Create VM with vsock.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "vsocktest",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--cpus",
        "1",
        "--memory",
        "256M",
        "--vsock",
        "--no-start",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm create failed.\nstderr: {stderr}"
    );

    // Get the UDS path from inspect before starting (it's in metadata).
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "vsocktest",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let uds_path = parsed["vsock"]["uds_path"]
        .as_str()
        .expect("no uds_path in inspect")
        .to_string();
    eprintln!("Expected UDS path: {uds_path}");

    // Start the VM.
    let output = common::ember(&["--state-dir", state, "vm", "start", "vsocktest"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm start failed.\nstderr: {stderr}"
    );

    // Give ember-vz time to set up the vsock bridge and UDS listener.
    std::thread::sleep(Duration::from_secs(3));

    // Verify the UDS file exists.
    let uds = std::path::Path::new(&uds_path);
    assert!(uds.exists(), "UDS file not found at {uds_path}");

    // Verify we can connect to the UDS.
    // ember-vz creates a UDS listener that accepts connections and bridges
    // them to guest port 1024. The connect should succeed even if no guest
    // listener is running (ember-vz accepts the connection, then the guest
    // connect may fail — but the UDS connect itself should work).
    let connect_result = UnixStream::connect(&uds_path);
    eprintln!("UDS connect result: {connect_result:?}");
    assert!(
        connect_result.is_ok(),
        "failed to connect to vsock UDS at {uds_path}: {}",
        connect_result.unwrap_err()
    );

    // Clean up.
    common::stop_and_delete_vm(state, "vsocktest");
}

// ---------------------------------------------------------------------------
// Linux end-to-end test (requires Firecracker + KVM)
// ---------------------------------------------------------------------------

/// Boot a VM with --vsock on Linux, verify the UDS appears.
///
/// Firecracker creates the vsock UDS directly (unlike macOS where ember-vz
/// manages it). Verifies the PUT /vsock API call succeeds and the UDS exists.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn vsock_uds_created_linux() {
    let env = common::TestEnv::with_image("vsock_e2e_linux");
    let state = env.state();

    common::linux::require_firecracker();
    let kernel = common::linux::ensure_kernel();

    // Create VM with vsock.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "vsocktest",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--cpus",
        "1",
        "--memory",
        "128M",
        "--vsock",
        "--no-start",
    ]);
    assert!(
        output.status.success(),
        "vm create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Get the UDS path from inspect.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "vsocktest",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let uds_path = parsed["vsock"]["uds_path"]
        .as_str()
        .expect("no uds_path in inspect")
        .to_string();

    // Start the VM (Firecracker creates the UDS via PUT /vsock).
    let output = common::ember(&["--state-dir", state, "vm", "start", "vsocktest"]);
    assert!(
        output.status.success(),
        "vm start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Firecracker creates the UDS synchronously during boot.
    std::thread::sleep(Duration::from_secs(2));

    // Verify the UDS file exists.
    let uds = std::path::Path::new(&uds_path);
    assert!(uds.exists(), "UDS file not found at {uds_path}");

    // Verify we can connect to the UDS.
    let connect_result = UnixStream::connect(&uds_path);
    assert!(
        connect_result.is_ok(),
        "failed to connect to vsock UDS at {uds_path}: {}",
        connect_result.unwrap_err()
    );

    // Clean up.
    common::stop_and_delete_vm(state, "vsocktest");
}
