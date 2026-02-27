//! Integration tests for `ember vm create`, `vm start`, `vm stop`, and `vm delete`.
//!
//! These tests require:
//! - Root privileges
//! - Working ZFS installation
//! - Network access (to pull OCI images from Docker Hub)
//! - `skopeo` installed
//!
//! Tests that start a VM additionally require:
//! - `firecracker` binary installed and in PATH
//! - A Linux kernel image (auto-downloaded to `/tmp/ember-test-vmlinux` on
//!   first run; override with `EMBER_TEST_KERNEL` env var)
//!
//! They are marked `#[ignore]` so `cargo test` skips them by default.
//!
//! To run:
//!   ./run-integration-tests.sh vm

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Shared helpers (same pattern as init.rs and image.rs)
// ---------------------------------------------------------------------------

/// Unique pool name per test to avoid collisions.
fn test_pool(name: &str) -> String {
    format!("embertest_{name}_{}", std::process::id())
}

/// Create a loopback file and attach it to a loop device.
fn create_loop_device(dir: &std::path::Path) -> (String, PathBuf) {
    create_loop_device_sized(dir, "512M")
}

/// Create a loopback file of the given size and attach it to a loop device.
fn create_loop_device_sized(dir: &std::path::Path, size: &str) -> (String, PathBuf) {
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

    let dev = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .to_string();
    (dev, file)
}

/// Detach a loop device (best-effort cleanup).
fn detach_loop_device(dev: &str) {
    let _ = Command::new("losetup").args(["-d", dev]).status();
}

/// Destroy a ZFS pool (best-effort cleanup).
fn destroy_pool(pool: &str) {
    let _ = Command::new("zpool")
        .args(["destroy", "-f", pool])
        .status();
}

/// Path to the ember binary built by cargo.
fn ember_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // remove test binary name
    if path.ends_with("deps") {
        path.pop(); // remove deps/
    }
    path.push("ember");
    path
}

/// Run ember with the given args, returning the Output.
fn ember(args: &[&str]) -> std::process::Output {
    Command::new(ember_bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute ember: {e}"))
}

/// RAII guard: destroys pool and detaches loop device on drop.
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

/// Assert that a ZFS dataset (zvol, filesystem, etc.) exists.
fn assert_dataset_exists(dataset: &str) {
    let output = Command::new("zfs")
        .args(["list", "-H", dataset])
        .output()
        .expect("failed to run zfs");
    assert!(output.status.success(), "expected dataset '{dataset}' to exist");
}

/// Assert that a ZFS dataset does NOT exist.
fn assert_dataset_absent(dataset: &str) {
    let output = Command::new("zfs")
        .args(["list", "-H", dataset])
        .output()
        .expect("failed to run zfs");
    assert!(
        !output.status.success(),
        "expected dataset '{dataset}' to NOT exist"
    );
}

/// Set up a ZFS pool, run `ember init`, and pull the alpine image.
/// Returns (pool_name, state_dir, cleanup_guard).
fn setup_pool_init_and_pull(
    test_name: &str,
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

    (pool, state_dir, cleanup)
}

/// Create a dummy kernel file (for tests that don't actually boot a VM).
fn create_dummy_kernel(dir: &std::path::Path) -> PathBuf {
    let kernel = dir.join("vmlinux-dummy");
    std::fs::write(&kernel, b"not a real kernel").unwrap();
    kernel
}

const KERNEL_CACHE_PATH: &str = "/tmp/ember-test-vmlinux";
/// Use the same Firecracker CI kernel that `ember vm create` downloads by
/// default.  The old quickstart kernel lacks cgroups and other features
/// required by systemd, so Ubuntu-based VMs would fail to boot properly.
const KERNEL_URL: &str =
    "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/x86_64/vmlinux-6.1.102";

/// Check that Docker is available for building images.
fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Set up a ZFS pool, run `ember init`, and build the ubuntu-vm image.
///
/// The ubuntu-vm image includes systemd, sshd, and networking tools —
/// everything needed for SSH and internet connectivity tests.
/// Requires Docker for the image build step.
///
/// Uses a 4 GB sparse file for the ZFS pool (ubuntu-vm is ~1-2 GB).
fn setup_pool_init_and_build_ubuntu(
    test_name: &str,
    tmp: &tempfile::TempDir,
) -> (String, PathBuf, PoolCleanup) {
    let pool = test_pool(test_name);
    let state_dir = tmp.path().join("state");
    let (loop_dev, _img) = create_loop_device_sized(tmp.path(), "4G");

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

    // Build ubuntu-vm image (includes systemd + sshd).
    let output = ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "image",
        "build",
        "ubuntu-vm",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "image build ubuntu-vm failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    (pool, state_dir, cleanup)
}

/// Check that Firecracker prerequisites are met: binary in PATH and /dev/kvm available.
/// Returns false (with a message) if anything is missing.
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

    if !std::path::Path::new("/dev/kvm").exists() {
        eprintln!("Skipping: /dev/kvm not available (no hardware virtualization)");
        return false;
    }

    true
}

/// Get a bootable kernel for Firecracker tests.
///
/// Resolution order:
/// 1. `EMBER_TEST_KERNEL` env var (explicit override)
/// 2. Cached download at `/tmp/ember-test-vmlinux`
/// 3. Fresh download from the Firecracker quickstart S3 bucket
///
/// Returns `None` if the download fails (test should skip gracefully).
fn ensure_kernel() -> Option<PathBuf> {
    // Honor explicit override.
    if let Ok(p) = std::env::var("EMBER_TEST_KERNEL") {
        let path = PathBuf::from(&p);
        assert!(
            path.exists(),
            "EMBER_TEST_KERNEL points to non-existent file: {p}"
        );
        return Some(path);
    }

    // Use cached download if present.
    let cache = PathBuf::from(KERNEL_CACHE_PATH);
    if cache.exists() {
        return Some(cache);
    }

    // Download to a unique temp file, then rename atomically. This avoids
    // interleaved output when multiple tests race through ensure_kernel().
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
            // Atomic rename — if another thread beat us, both files are valid.
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

// ---------------------------------------------------------------------------
// Tests that only need ZFS (no Firecracker required)
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn create_vm_shows_in_list_and_inspect() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_init_and_pull("vmcreate", &tmp);
    let kernel = create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create a VM (--no-start so we don't need Firecracker).
    let output = ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "testvm",
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
    assert!(
        stdout.contains("created successfully"),
        "expected success message: {stdout}"
    );

    // Verify vm list shows the VM.
    let list_output = ember(&["--state-dir", state, "vm", "list"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(list_output.status.success());
    assert!(
        list_stdout.contains("testvm"),
        "expected 'testvm' in list: {list_stdout}"
    );
    assert!(
        list_stdout.contains("created"),
        "expected 'created' status in list: {list_stdout}"
    );

    // Verify vm inspect shows correct details.
    let inspect_output = ember(&["--state-dir", state, "vm", "inspect", "testvm"]);
    let inspect_stdout = String::from_utf8_lossy(&inspect_output.stdout);
    assert!(inspect_output.status.success());
    assert!(inspect_stdout.contains("testvm"));
    assert!(inspect_stdout.contains("created"));
    assert!(inspect_stdout.contains("alpine"));

    // Verify JSON inspect output.
    let json_output = ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "testvm",
        "--format",
        "json",
    ]);
    let json_stdout = String::from_utf8_lossy(&json_output.stdout);
    assert!(json_output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {json_stdout}"));
    assert_eq!(parsed["name"], "testvm");
    assert_eq!(parsed["status"], "created");
}

#[test]
#[ignore]
fn create_duplicate_vm_name_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_init_and_pull("vmdup", &tmp);
    let kernel = create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create first VM.
    let output1 = ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "dupvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--no-start",
    ]);
    assert!(
        output1.status.success(),
        "first create failed: {}",
        String::from_utf8_lossy(&output1.stderr)
    );

    // Try creating with the same name — should fail.
    let output2 = ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "dupvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--no-start",
    ]);
    assert!(
        !output2.status.success(),
        "expected duplicate create to fail"
    );
    let stderr = String::from_utf8_lossy(&output2.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' error: {stderr}"
    );
}

#[test]
#[ignore]
fn delete_created_vm_cleans_up_zvol_and_state() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = setup_pool_init_and_pull("vmdel", &tmp);
    let kernel = create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create a VM.
    let output = ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "delvm",
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

    let vm_zvol = format!("{pool}/ember/vms/delvm");
    assert_dataset_exists(&vm_zvol);

    // Delete it.
    let del_output = ember(&["--state-dir", state, "vm", "delete", "delvm"]);
    let del_stdout = String::from_utf8_lossy(&del_output.stdout);
    let del_stderr = String::from_utf8_lossy(&del_output.stderr);
    assert!(
        del_output.status.success(),
        "vm delete failed.\nstdout: {del_stdout}\nstderr: {del_stderr}"
    );
    assert!(
        del_stdout.contains("deleted"),
        "expected 'deleted' in output: {del_stdout}"
    );

    // Verify zvol is gone.
    assert_dataset_absent(&vm_zvol);

    // Verify vm list is empty.
    let list_output = ember(&["--state-dir", state, "vm", "list"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_stdout.contains("No VMs found"),
        "expected empty vm list: {list_stdout}"
    );
}

#[test]
#[ignore]
fn stop_created_vm_fails_with_wrong_state() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_init_and_pull("vmstopstate", &tmp);
    let kernel = create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create a VM but don't start it.
    let output = ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "stoptest",
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

    // Try to stop a created (not running) VM — should fail.
    let stop_output = ember(&["--state-dir", state, "vm", "stop", "stoptest"]);
    assert!(
        !stop_output.status.success(),
        "expected stop to fail for non-running VM"
    );
    let stderr = String::from_utf8_lossy(&stop_output.stderr);
    assert!(
        stderr.contains("running or paused"),
        "expected state error: {stderr}"
    );
}

#[test]
#[ignore]
fn delete_nonexistent_vm_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = setup_pool_init_and_pull("vmdelnoexist", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = ember(&["--state-dir", state, "vm", "delete", "nosuchvm"]);
    assert!(
        !output.status.success(),
        "expected delete of nonexistent VM to fail"
    );
}

// ---------------------------------------------------------------------------
// Full lifecycle test (requires Firecracker + kernel)
// ---------------------------------------------------------------------------

/// Full VM lifecycle: create → start → verify running → stop → delete.
///
/// Requires `firecracker` in PATH and a bootable kernel (auto-downloaded
/// or overridden via `EMBER_TEST_KERNEL`). Skips if prerequisites are missing.
#[test]
#[ignore]
fn full_vm_lifecycle_start_stop_delete() {
    if !firecracker_available() {
        return;
    }

    let kernel_path = match ensure_kernel() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = setup_pool_init_and_pull("vmlifecycle", &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // -- Create --
    let create_output = ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "lifecyclevm",
        "--image",
        "alpine:latest",
        "--cpus",
        "1",
        "--memory",
        "128",
        "--kernel",
        kernel,
        "--no-start",
    ]);
    let stdout = String::from_utf8_lossy(&create_output.stdout);
    let stderr = String::from_utf8_lossy(&create_output.stderr);
    assert!(
        create_output.status.success(),
        "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // -- Start --
    let start_output = ember(&["--state-dir", state, "vm", "start", "lifecyclevm"]);
    let start_stdout = String::from_utf8_lossy(&start_output.stdout);
    let start_stderr = String::from_utf8_lossy(&start_output.stderr);
    assert!(
        start_output.status.success(),
        "vm start failed.\nstdout: {start_stdout}\nstderr: {start_stderr}"
    );
    assert!(
        start_stdout.contains("started"),
        "expected 'started' in output: {start_stdout}"
    );

    // -- Verify Running via inspect --
    let inspect_output = ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "lifecyclevm",
        "--format",
        "json",
    ]);
    assert!(inspect_output.status.success());
    let inspect_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&inspect_output.stdout))
            .expect("failed to parse inspect JSON");
    assert_eq!(
        inspect_json["status"], "running",
        "expected status 'running', got: {}",
        inspect_json["status"]
    );

    // Verify the Firecracker process is actually alive.
    let pid = inspect_json["pid"]
        .as_u64()
        .expect("expected numeric PID in inspect output");
    let proc_alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
    assert!(
        proc_alive,
        "expected Firecracker process (pid {pid}) to be alive"
    );

    // -- Stop --
    let stop_output = ember(&[
        "--state-dir",
        state,
        "vm",
        "stop",
        "lifecyclevm",
        "--force",
    ]);
    let stop_stdout = String::from_utf8_lossy(&stop_output.stdout);
    let stop_stderr = String::from_utf8_lossy(&stop_output.stderr);
    assert!(
        stop_output.status.success(),
        "vm stop failed.\nstdout: {stop_stdout}\nstderr: {stop_stderr}"
    );
    assert!(
        stop_stdout.contains("stopped"),
        "expected 'stopped' in output: {stop_stdout}"
    );

    // Verify status is Stopped.
    let inspect2 = ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "lifecyclevm",
        "--format",
        "json",
    ]);
    assert!(inspect2.status.success());
    let inspect2_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&inspect2.stdout))
            .expect("failed to parse inspect JSON after stop");
    assert_eq!(inspect2_json["status"], "stopped");
    assert!(
        inspect2_json["pid"].is_null(),
        "expected pid to be null after stop"
    );

    // Verify the process is dead.
    let proc_dead = !std::path::Path::new(&format!("/proc/{pid}")).exists();
    assert!(
        proc_dead,
        "expected Firecracker process (pid {pid}) to be dead after stop"
    );

    // -- Delete --
    let del_output = ember(&["--state-dir", state, "vm", "delete", "lifecyclevm"]);
    let del_stdout = String::from_utf8_lossy(&del_output.stdout);
    let del_stderr = String::from_utf8_lossy(&del_output.stderr);
    assert!(
        del_output.status.success(),
        "vm delete failed.\nstdout: {del_stdout}\nstderr: {del_stderr}"
    );

    // Verify zvol is gone.
    assert_dataset_absent(&format!("{pool}/ember/vms/lifecyclevm"));

    // Verify VM no longer in list.
    let list_output = ember(&["--state-dir", state, "vm", "list"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_stdout.contains("No VMs found"),
        "expected empty vm list after delete: {list_stdout}"
    );
}

/// Delete a running VM requires --force.
///
/// Same prerequisites as `full_vm_lifecycle_start_stop_delete`.
#[test]
#[ignore]
fn delete_running_vm_requires_force() {
    if !firecracker_available() {
        return;
    }

    let kernel_path = match ensure_kernel() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = setup_pool_init_and_pull("vmdelrunning", &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // Create and start a VM.
    let create_output = ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "runningvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel,
        "--no-start",
    ]);
    assert!(
        create_output.status.success(),
        "vm create failed: {}",
        String::from_utf8_lossy(&create_output.stderr)
    );

    let start_output = ember(&["--state-dir", state, "vm", "start", "runningvm"]);
    assert!(
        start_output.status.success(),
        "vm start failed: {}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // Try to delete without --force — should fail.
    let del_output = ember(&["--state-dir", state, "vm", "delete", "runningvm"]);
    assert!(
        !del_output.status.success(),
        "expected delete of running VM to fail without --force"
    );
    let stderr = String::from_utf8_lossy(&del_output.stderr);
    assert!(
        stderr.contains("--force"),
        "expected error mentioning --force: {stderr}"
    );

    // Delete with --force — should succeed and kill the process.
    let force_output = ember(&[
        "--state-dir",
        state,
        "vm",
        "delete",
        "runningvm",
        "--force",
    ]);
    let force_stdout = String::from_utf8_lossy(&force_output.stdout);
    let force_stderr = String::from_utf8_lossy(&force_output.stderr);
    assert!(
        force_output.status.success(),
        "vm delete --force failed.\nstdout: {force_stdout}\nstderr: {force_stderr}"
    );

    // Verify zvol is gone.
    assert_dataset_absent(&format!("{pool}/ember/vms/runningvm"));
}

// ---------------------------------------------------------------------------
// Networking test (requires Firecracker + kernel + network access)
// ---------------------------------------------------------------------------

/// Resolve the SSH private key path for the invoking user.
///
/// Under `sudo`, uses `SUDO_USER` to find the real user's key.
/// This must match what `image::inject::default_ssh_pubkey_path()` picks,
/// since the corresponding public key is injected into the guest.
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

/// Try to SSH into the guest and run a command.
///
/// Returns `Ok(stdout)` on success, `Err(stderr)` on failure.
fn ssh_exec(guest_ip: &str, key_path: &Path, command: &str) -> Result<String, String> {
    let output = Command::new("ssh")
        .args([
            "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null",
            "-o", "ConnectTimeout=5",
            "-o", "BatchMode=yes",
            "-o", "LogLevel=ERROR",
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

/// Wait for SSH to become reachable on the guest.
///
/// Retries with exponential backoff up to ~60 seconds total.
/// Returns `true` if SSH became reachable, `false` on timeout.
fn wait_for_ssh(guest_ip: &str, key_path: &Path) -> bool {
    let delays_ms = [
        500, 1000, 1000, 2000, 2000, 3000, 3000, 5000, 5000, 5000,
        5000, 5000, 5000, 5000, 5000, 5000,
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

/// Full networking test: start VM → verify TAP + iptables + host-to-guest ping
/// → SSH into guest → verify internet from guest → stop → delete.
///
/// Uses the `ubuntu-vm` image (built via Docker) which includes systemd,
/// openssh-server, and networking tools — everything needed for proper SSH
/// and internet connectivity testing.
///
/// Requires:
/// - `firecracker` in PATH, `/dev/kvm` available
/// - `docker` for building the ubuntu-vm image
/// - Bootable kernel (auto-downloaded or `EMBER_TEST_KERNEL`)
/// - SSH key pair for the invoking user
/// - Network access (host must be able to reach the internet)
///
/// Skips if prerequisites are missing.
#[test]
#[ignore]
fn networking_ssh_and_internet() {
    if !firecracker_available() {
        return;
    }

    if !docker_available() {
        eprintln!("Skipping: docker not available (needed to build ubuntu-vm image)");
        return;
    }

    let kernel_path = match ensure_kernel() {
        Some(p) => p,
        None => return,
    };

    let ssh_key = match ssh_private_key_path() {
        Some(p) => p,
        None => {
            eprintln!("Skipping: no SSH private key found for the invoking user");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = setup_pool_init_and_build_ubuntu("vmnetwork", &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // -- Create VM --
    // Ubuntu with systemd needs more memory than Alpine with busybox init.
    let create_output = ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "netvm",
        "--image",
        "ubuntu-vm",
        "--cpus",
        "1",
        "--memory",
        "512",
        "--kernel",
        kernel,
        "--no-start",
    ]);
    let stdout = String::from_utf8_lossy(&create_output.stdout);
    let stderr = String::from_utf8_lossy(&create_output.stderr);
    assert!(
        create_output.status.success(),
        "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // -- Start VM --
    let start_output = ember(&["--state-dir", state, "vm", "start", "netvm"]);
    let start_stdout = String::from_utf8_lossy(&start_output.stdout);
    let start_stderr = String::from_utf8_lossy(&start_output.stderr);
    assert!(
        start_output.status.success(),
        "vm start failed.\nstdout: {start_stdout}\nstderr: {start_stderr}"
    );

    // -- Inspect: verify network metadata --
    let inspect_output = ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "netvm",
        "--format",
        "json",
    ]);
    assert!(inspect_output.status.success());
    let inspect_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&inspect_output.stdout))
            .expect("failed to parse inspect JSON");

    assert_eq!(inspect_json["status"], "running");
    assert!(inspect_json["pid"].is_u64(), "expected numeric PID");

    let network = &inspect_json["network"];
    assert!(
        !network.is_null(),
        "expected network info in inspect output"
    );

    let tap_device = network["tap_device"]
        .as_str()
        .expect("expected tap_device string");
    let guest_ip = network["guest_ip"]
        .as_str()
        .expect("expected guest_ip string");
    let host_ip = network["host_ip"]
        .as_str()
        .expect("expected host_ip string");

    assert!(
        tap_device.starts_with("em-"),
        "TAP device should start with 'em-', got: {tap_device}"
    );
    assert!(
        !guest_ip.is_empty(),
        "guest_ip should not be empty"
    );
    assert!(
        !host_ip.is_empty(),
        "host_ip should not be empty"
    );

    eprintln!("Network info: TAP={tap_device} host={host_ip} guest={guest_ip}");

    // -- Verify TAP device exists on host --
    let ip_link = Command::new("ip")
        .args(["link", "show", tap_device])
        .output()
        .expect("failed to run ip link show");
    assert!(
        ip_link.status.success(),
        "TAP device '{tap_device}' not found on host: {}",
        String::from_utf8_lossy(&ip_link.stderr)
    );

    // -- Verify iptables NAT rules --
    let iptables_nat = Command::new("iptables")
        .args(["-t", "nat", "-S", "POSTROUTING"])
        .output()
        .expect("failed to run iptables");
    let nat_rules = String::from_utf8_lossy(&iptables_nat.stdout);
    assert!(
        nat_rules.contains(guest_ip),
        "expected MASQUERADE rule for {guest_ip} in NAT table:\n{nat_rules}"
    );

    // -- Verify FORWARD chain rules --
    let iptables_fwd = Command::new("iptables")
        .args(["-S", "FORWARD"])
        .output()
        .expect("failed to run iptables");
    let fwd_rules = String::from_utf8_lossy(&iptables_fwd.stdout);
    assert!(
        fwd_rules.contains(tap_device),
        "expected FORWARD rules mentioning {tap_device}:\n{fwd_rules}"
    );

    // -- Ping guest from host --
    // The guest needs a moment to boot and configure its network via the
    // kernel ip= parameter. Retry ping with short delays.
    let mut ping_ok = false;
    for attempt in 1..=20 {
        let ping = Command::new("ping")
            .args(["-c", "1", "-W", "1", guest_ip])
            .output()
            .expect("failed to run ping");
        if ping.status.success() {
            eprintln!("Host-to-guest ping succeeded on attempt {attempt}");
            ping_ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert!(
        ping_ok,
        "failed to ping guest at {guest_ip} from host after 20 attempts"
    );

    // -- SSH into guest --
    // Ubuntu with systemd + sshd needs more time to boot than Alpine.
    eprintln!("Waiting for SSH to become available...");
    assert!(
        wait_for_ssh(guest_ip, &ssh_key),
        "SSH not reachable at {guest_ip}:22 after timeout"
    );

    // Run a simple command to verify SSH exec works.
    let hostname_result = ssh_exec(guest_ip, &ssh_key, "hostname");
    assert!(
        hostname_result.is_ok(),
        "SSH command 'hostname' failed: {:?}",
        hostname_result.err()
    );
    let hostname = hostname_result.unwrap();
    eprintln!("Guest hostname: {hostname}");

    // -- Verify internet from guest --
    // Ubuntu has both wget and ping available.
    let inet_result = ssh_exec(
        guest_ip,
        &ssh_key,
        "wget -q -O /dev/null -T 5 http://example.com && echo OK",
    );
    match &inet_result {
        Ok(out) => {
            assert!(
                out.contains("OK"),
                "expected 'OK' from wget, got: {out}"
            );
            eprintln!("Guest internet access verified (wget http://example.com)");
        }
        Err(e) => {
            // wget might fail due to DNS; try ping as fallback.
            eprintln!("wget failed ({e}), trying ping...");
            let ping_result = ssh_exec(guest_ip, &ssh_key, "ping -c 1 -W 5 8.8.8.8");
            assert!(
                ping_result.is_ok(),
                "Guest cannot reach the internet. wget: {e}, ping: {:?}",
                ping_result.err()
            );
            eprintln!("Guest internet access verified (ping 8.8.8.8)");
        }
    }

    // -- Stop VM --
    let stop_output = ember(&[
        "--state-dir",
        state,
        "vm",
        "stop",
        "netvm",
        "--force",
    ]);
    let stop_stdout = String::from_utf8_lossy(&stop_output.stdout);
    let stop_stderr = String::from_utf8_lossy(&stop_output.stderr);
    assert!(
        stop_output.status.success(),
        "vm stop failed.\nstdout: {stop_stdout}\nstderr: {stop_stderr}"
    );

    // -- Verify network cleanup after stop --
    let ip_link_after = Command::new("ip")
        .args(["link", "show", tap_device])
        .output()
        .expect("failed to run ip link show");
    assert!(
        !ip_link_after.status.success(),
        "TAP device '{tap_device}' should be gone after stop"
    );

    let iptables_nat_after = Command::new("iptables")
        .args(["-t", "nat", "-S", "POSTROUTING"])
        .output()
        .expect("failed to run iptables");
    let nat_rules_after = String::from_utf8_lossy(&iptables_nat_after.stdout);
    assert!(
        !nat_rules_after.contains(guest_ip),
        "MASQUERADE rule for {guest_ip} should be gone after stop:\n{nat_rules_after}"
    );

    // -- Delete VM --
    let del_output = ember(&["--state-dir", state, "vm", "delete", "netvm"]);
    assert!(
        del_output.status.success(),
        "vm delete failed: {}",
        String::from_utf8_lossy(&del_output.stderr)
    );

    assert_dataset_absent(&format!("{pool}/ember/vms/netvm"));
    eprintln!("Networking test complete.");
}
