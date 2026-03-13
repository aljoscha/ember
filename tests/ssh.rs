//! Integration tests for `ember exec` and `ember cp` (Linux-only).
//!
//! These tests require:
#![cfg(target_os = "linux")]
//!
//! - Root privileges
//! - Working ZFS installation
//! - `firecracker` binary in PATH + `/dev/kvm`
//! - `docker` for building the `ubuntu-slim` image (systemd + sshd)
//! - Bootable kernel (auto-downloaded or `EMBER_TEST_KERNEL`)
//! - SSH key pair for the invoking user
//! - Network access
//!
//! They are marked `#[ignore]` so `cargo test` skips them by default.
//!
//! To run:
//!   ./run-integration-tests.sh ssh

#[allow(dead_code)]
mod common;

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Helpers: start a VM and return info needed for SSH tests
// ---------------------------------------------------------------------------

struct RunningVm {
    state_dir: PathBuf,
    vm_name: String,
    // Fields are dropped in declaration order: vm first, then pool, then tmpdir.
    _cleanup: common::linux::PoolCleanup,
    _tmp: tempfile::TempDir,
}

impl Drop for RunningVm {
    fn drop(&mut self) {
        common::linux::stop_and_delete_vm(&self.state_dir, &self.vm_name);
    }
}

/// Spin up a running ubuntu-slim. Returns the state_dir and guest_ip.
///
/// Skips (returns None) if prerequisites are missing.
fn start_ubuntu_vm(test_name: &str, vm_name: &str) -> Option<RunningVm> {
    if !common::linux::firecracker_available() {
        return None;
    }
    if !common::linux::docker_available() {
        eprintln!("Skipping: docker not available (needed to build ubuntu-slim image)");
        return None;
    }
    let kernel_path = common::linux::ensure_kernel()?;
    let ssh_key = common::linux::ssh_private_key_path();
    if ssh_key.is_none() {
        eprintln!("Skipping: no SSH private key found for the invoking user");
        return None;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, cleanup) =
        common::linux::setup_pool_init_and_build_ubuntu(test_name, &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // Create VM.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        vm_name,
        "--image",
        "ubuntu-slim",
        "--cpus",
        "1",
        "--memory",
        "512M",
        "--kernel",
        kernel,
        "--no-start",
    ]);
    assert!(
        output.status.success(),
        "vm create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Start VM.
    let output = common::ember(&["--state-dir", state, "vm", "start", vm_name]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm start failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Extract guest IP from inspect.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        vm_name,
        "--format",
        "json",
    ]);
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("failed to parse inspect JSON");
    let guest_ip = json["network"]["guest_ip"]
        .as_str()
        .expect("expected guest_ip")
        .to_string();

    // Wait for SSH.
    let ssh_key = ssh_key.unwrap();
    eprintln!("Waiting for SSH to become available at {guest_ip}...");
    assert!(
        common::linux::wait_for_ssh(&guest_ip, &ssh_key),
        "SSH not reachable at {guest_ip}:22 after timeout"
    );

    Some(RunningVm {
        state_dir,
        vm_name: vm_name.to_string(),
        _cleanup: cleanup,
        _tmp: tmp,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test `ember exec`: run a command on a running VM and verify output.
#[test]
#[ignore]
fn exec_command_returns_stdout() {
    let vm = match start_ubuntu_vm("sshexec", "execvm") {
        Some(v) => v,
        None => return,
    };
    let state = vm.state_dir.to_str().unwrap();

    // Run a simple command via `ember exec`.
    let output = common::ember(&[
        "--state-dir",
        state,
        "exec",
        "execvm",
        "--",
        "echo",
        "hello-from-ember",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "ember exec failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("hello-from-ember"),
        "expected 'hello-from-ember' in exec output: {stdout}"
    );
    eprintln!("ember exec echo: {}", stdout.trim());

    // Run a command that produces meaningful output.
    let output = common::ember(&["--state-dir", state, "exec", "execvm", "--", "uname", "-r"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "ember exec uname failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let kernel_version = stdout.trim();
    assert!(
        !kernel_version.is_empty(),
        "expected non-empty kernel version"
    );
    eprintln!("Guest kernel: {kernel_version}");

    // Run a command that fails — verify non-zero exit code is propagated.
    let output = common::ember(&["--state-dir", state, "exec", "execvm", "--", "false"]);
    assert!(
        !output.status.success(),
        "expected 'false' command to return non-zero exit code"
    );

    eprintln!("exec test complete.");
}

/// Test `ember cp`: upload a file to VM, then download it back.
#[test]
#[ignore]
fn cp_upload_and_download() {
    let vm = match start_ubuntu_vm("sshcp", "cpvm") {
        Some(v) => v,
        None => return,
    };
    let state = vm.state_dir.to_str().unwrap();
    let tmp_dir = tempfile::tempdir().unwrap();

    // Create a local file with known content.
    let test_content = "ember-cp-test-content-42\nline two\n";
    let local_src = tmp_dir.path().join("upload.txt");
    std::fs::write(&local_src, test_content).unwrap();

    // Upload: local → VM.
    let output = common::ember(&[
        "--state-dir",
        state,
        "cp",
        local_src.to_str().unwrap(),
        "cpvm:/tmp/uploaded.txt",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "ember cp upload failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Verify the file arrived on the guest via exec.
    let output = common::ember(&[
        "--state-dir",
        state,
        "exec",
        "cpvm",
        "--",
        "cat",
        "/tmp/uploaded.txt",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "failed to cat uploaded file: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.as_ref(),
        test_content,
        "uploaded file content mismatch"
    );
    eprintln!("Upload verified: content matches on guest");

    // Download: VM → local.
    let local_dst = tmp_dir.path().join("downloaded.txt");
    let output = common::ember(&[
        "--state-dir",
        state,
        "cp",
        "cpvm:/tmp/uploaded.txt",
        local_dst.to_str().unwrap(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "ember cp download failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Verify downloaded content matches.
    let downloaded = std::fs::read_to_string(&local_dst).unwrap();
    assert_eq!(downloaded, test_content, "downloaded file content mismatch");
    eprintln!("Download verified: content matches locally");

    eprintln!("cp test complete.");
}

/// Test `ember exec` against a non-running VM fails with a clear error.
#[test]
#[ignore]
fn exec_on_stopped_vm_fails() {
    // This test only needs ZFS (no Firecracker) — create a VM but don't start it.
    let tmp = tempfile::tempdir().unwrap();
    let pool = common::linux::test_pool("sshexecstopped");
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = common::linux::create_loop_device_sized(tmp.path(), "512M");

    let _cleanup = common::linux::PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    // Init.
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

    // Pull alpine.
    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "pull",
        "alpine:latest",
    ]);
    assert!(
        output.status.success(),
        "image pull failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Create dummy kernel for --no-start.
    let kernel = tmp.path().join("vmlinux-dummy");
    std::fs::write(&kernel, b"not a real kernel").unwrap();

    // Create VM (not started).
    let state = state_dir.to_str().unwrap();
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "stoppedvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--no-start",
    ]);
    assert!(
        output.status.success(),
        "vm create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Try exec on a non-running VM — should fail.
    let output = common::ember(&[
        "--state-dir",
        state,
        "exec",
        "stoppedvm",
        "--",
        "echo",
        "hello",
    ]);
    assert!(
        !output.status.success(),
        "expected exec on stopped VM to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("created"),
        "expected error about VM not running: {stderr}"
    );

    // Try cp on a non-running VM — should also fail.
    let local_file = tmp.path().join("test.txt");
    std::fs::write(&local_file, "test").unwrap();
    let output = common::ember(&[
        "--state-dir",
        state,
        "cp",
        local_file.to_str().unwrap(),
        "stoppedvm:/tmp/test.txt",
    ]);
    assert!(
        !output.status.success(),
        "expected cp on stopped VM to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("created"),
        "expected error about VM not running: {stderr}"
    );
}
