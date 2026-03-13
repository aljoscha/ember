//! Integration tests for `ember exec` and `ember cp`.
//!
//! The `exec_on_stopped_vm_fails` test is cross-platform and uses
//! `TestEnv::with_vm()` (no hypervisor needed).
//!
//! The running-VM SSH tests (`exec_command_returns_stdout`,
//! `cp_upload_and_download`) use `TestEnv::with_running_ssh_vm()` which
//! boots ubuntu-slim (with sshd). These require Docker + a hypervisor
//! (Firecracker on Linux, ember-vz on macOS).
//!
//! To run:
//!   ./run-integration-tests.sh ssh

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Cross-platform tests (no hypervisor needed)
// ---------------------------------------------------------------------------

/// `ember exec` and `ember cp` against a stopped VM should fail with a clear error.
#[test]
#[ignore]
fn exec_on_stopped_vm_fails() {
    let env = common::TestEnv::with_vm("sshexecstopped", "stoppedvm");
    let state = env.state();

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
    let local_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(local_file.path(), "test").unwrap();
    let output = common::ember(&[
        "--state-dir",
        state,
        "cp",
        local_file.path().to_str().unwrap(),
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

// ---------------------------------------------------------------------------
// Cross-platform tests (require running VM with SSH)
// ---------------------------------------------------------------------------

/// Test `ember exec`: run a command on a running VM and verify output.
///
/// Uses ubuntu-slim (built via Docker) which includes sshd.
/// Requires hypervisor + Docker.
#[test]
#[ignore]
fn exec_command_returns_stdout() {
    let env = common::TestEnv::with_running_ssh_vm("sshexec", "execvm");
    let state = env.state();

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

    // Cleanup.
    common::stop_and_delete_vm(state, "execvm");
}

/// Test `ember cp`: upload a file to VM, then download it back.
///
/// Uses ubuntu-slim (built via Docker) which includes sshd.
/// Requires hypervisor + Docker.
#[test]
#[ignore]
fn cp_upload_and_download() {
    let env = common::TestEnv::with_running_ssh_vm("sshcp", "cpvm");
    let state = env.state();
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

    // Cleanup.
    common::stop_and_delete_vm(state, "cpvm");
}
