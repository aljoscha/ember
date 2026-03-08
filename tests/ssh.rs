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

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Shared helpers (same pattern as vm.rs, init.rs, image.rs)
// ---------------------------------------------------------------------------

fn test_pool(name: &str) -> String {
    format!("embertest_{name}_{}", std::process::id())
}

fn create_loop_device_sized(dir: &Path, size: &str) -> (String, PathBuf) {
    let file = dir.join("pool.img");

    let status = Command::new("truncate")
        .args(["-s", size])
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

fn setup_pool_init_and_build_ubuntu(
    test_name: &str,
    tmp: &tempfile::TempDir,
) -> (String, PathBuf, PoolCleanup) {
    let pool = test_pool(test_name);
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = create_loop_device_sized(tmp.path(), "8G");

    let cleanup = PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

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

    let dockerfile = format!(
        "{}/images/Dockerfile.ubuntu-slim",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "build",
        "ubuntu-slim",
        "-f",
        &dockerfile,
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "image build ubuntu-slim failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    (pool, state_dir, cleanup)
}

fn firecracker_available() -> bool {
    let fc = Command::new("which")
        .arg("firecracker")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !fc {
        eprintln!("Skipping: firecracker not found in PATH");
        return false;
    }

    if !Path::new("/dev/kvm").exists() {
        eprintln!("Skipping: /dev/kvm not available (no hardware virtualization)");
        return false;
    }

    true
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

const KERNEL_CACHE_PATH: &str = "/tmp/ember-test-vmlinux";
const KERNEL_URL: &str =
    "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/x86_64/vmlinux-6.1.102";

fn ensure_kernel() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("EMBER_TEST_KERNEL") {
        let path = PathBuf::from(&p);
        assert!(
            path.exists(),
            "EMBER_TEST_KERNEL points to non-existent file: {p}"
        );
        return Some(path);
    }

    let cache = PathBuf::from(KERNEL_CACHE_PATH);
    if cache.exists() {
        return Some(cache);
    }

    let tmp = PathBuf::from(format!(
        "{KERNEL_CACHE_PATH}.{:?}",
        std::thread::current().id()
    ));
    eprintln!("Downloading Firecracker kernel from {KERNEL_URL}...");
    let status = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&tmp)
        .arg(KERNEL_URL)
        .status();

    match status {
        Ok(s) if s.success() => {
            let _ = std::fs::rename(&tmp, &cache);
            eprintln!("Kernel cached at {KERNEL_CACHE_PATH}");
            Some(cache)
        }
        _ => {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("Failed to download kernel — skipping Firecracker tests");
            None
        }
    }
}

fn ssh_private_key_path() -> Option<PathBuf> {
    let home = if let Ok(user) = std::env::var("SUDO_USER") {
        let output = Command::new("sh")
            .args(["-c", &format!("eval echo ~{user}")])
            .output()
            .ok()?;
        PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        PathBuf::from(std::env::var("HOME").ok()?)
    };

    let ssh_dir = home.join(".ssh");
    for name in &["id_ed25519", "id_ecdsa", "id_rsa"] {
        let path = ssh_dir.join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

fn ssh_exec(guest_ip: &str, key_path: &Path, command: &str) -> Result<String, String> {
    let output = Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "BatchMode=yes",
            "-o",
            "LogLevel=ERROR",
            "-i",
        ])
        .arg(key_path)
        .arg(format!("root@{guest_ip}"))
        .arg(command)
        .output()
        .map_err(|e| format!("failed to execute ssh: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn wait_for_ssh(guest_ip: &str, key_path: &Path) -> bool {
    let delays_ms = [
        500, 1000, 1000, 2000, 2000, 3000, 3000, 5000, 5000, 5000, 5000, 5000, 5000, 5000, 5000,
        5000,
    ];

    for (i, delay) in delays_ms.iter().enumerate() {
        eprintln!(
            "  SSH attempt {}/{}: connecting to {guest_ip}...",
            i + 1,
            delays_ms.len()
        );

        match ssh_exec(guest_ip, key_path, "true") {
            Ok(_) => {
                eprintln!("  SSH connected on attempt {}", i + 1);
                return true;
            }
            Err(e) => {
                eprintln!("  SSH attempt {} failed: {e}", i + 1);
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(*delay));
    }

    false
}

// ---------------------------------------------------------------------------
// Helpers: start a VM and return info needed for SSH tests
// ---------------------------------------------------------------------------

struct RunningVm {
    state_dir: PathBuf,
    vm_name: String,
    // Fields are dropped in declaration order: vm first, then pool, then tmpdir.
    _cleanup: PoolCleanup,
    _tmp: tempfile::TempDir,
}

impl Drop for RunningVm {
    fn drop(&mut self) {
        stop_and_delete_vm(&self.state_dir, &self.vm_name);
    }
}

/// Spin up a running ubuntu-slim. Returns the state_dir and guest_ip.
///
/// Skips (returns None) if prerequisites are missing.
fn start_ubuntu_vm(test_name: &str, vm_name: &str) -> Option<RunningVm> {
    if !firecracker_available() {
        return None;
    }
    if !docker_available() {
        eprintln!("Skipping: docker not available (needed to build ubuntu-slim image)");
        return None;
    }
    let kernel_path = ensure_kernel()?;
    let ssh_key = ssh_private_key_path();
    if ssh_key.is_none() {
        eprintln!("Skipping: no SSH private key found for the invoking user");
        return None;
    }

    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, cleanup) = setup_pool_init_and_build_ubuntu(test_name, &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // Create VM.
    let output = ember(&[
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
    let output = ember(&["--state-dir", state, "vm", "start", vm_name]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm start failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Extract guest IP from inspect.
    let output = ember(&[
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
        wait_for_ssh(&guest_ip, &ssh_key),
        "SSH not reachable at {guest_ip}:22 after timeout"
    );

    Some(RunningVm {
        state_dir,
        vm_name: vm_name.to_string(),
        _cleanup: cleanup,
        _tmp: tmp,
    })
}

/// Stop and delete a VM (best-effort cleanup).
fn stop_and_delete_vm(state_dir: &Path, vm_name: &str) {
    let state = state_dir.to_str().unwrap();
    let _ = ember(&["--state-dir", state, "vm", "stop", vm_name, "--force"]);
    let _ = ember(&["--state-dir", state, "vm", "delete", vm_name, "--force"]);
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
    let output = ember(&[
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
    let output = ember(&["--state-dir", state, "exec", "execvm", "--", "uname", "-r"]);
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
    let output = ember(&["--state-dir", state, "exec", "execvm", "--", "false"]);
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
    let output = ember(&[
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
    let output = ember(&[
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
    let output = ember(&[
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
    let pool = test_pool("sshexecstopped");
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = create_loop_device_sized(tmp.path(), "512M");

    let _cleanup = PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
    };

    // Init.
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

    // Pull alpine.
    let output = ember(&[
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
    let output = ember(&[
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
    let output = ember(&[
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
    let output = ember(&[
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
