//! Shared Linux test helpers for integration tests.
//!
//! Provides ZFS pool/loopback/zvol utilities, Firecracker availability checks,
//! kernel download, SSH helpers, and composite setup functions.
//!
//! These are extracted from the individual Linux test files to eliminate
//! ~500 lines of duplication. All functions are `pub` so test files can use
//! them via `common::linux::`.

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Pool / loopback device helpers
// ---------------------------------------------------------------------------

/// Unique ZFS pool name per test to avoid collisions.
///
/// Includes the PID so parallel test runs don't interfere.
pub fn test_pool(name: &str) -> String {
    format!("embertest_{name}_{}", std::process::id())
}

/// Create a 512 MB loopback file and attach it to a loop device.
/// Returns (loop_device_path, backing_file_path).
pub fn create_loop_device(dir: &Path) -> (String, PathBuf) {
    create_loop_device_sized(dir, "512M")
}

/// Create a loopback file of the given size and attach it to a loop device.
/// Returns (loop_device_path, backing_file_path).
///
/// The file is sparse, so it only consumes disk space as data is written.
pub fn create_loop_device_sized(dir: &Path, size: &str) -> (String, PathBuf) {
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

/// Detach a loop device (best-effort cleanup).
pub fn detach_loop_device(dev: &str) {
    let _ = Command::new("losetup").args(["-d", dev]).status();
}

/// Destroy a ZFS pool (best-effort cleanup).
pub fn destroy_pool(pool: &str) {
    let _ = Command::new("zpool").args(["destroy", "-f", pool]).status();
}

/// RAII guard: destroys ZFS pool and detaches loop device on drop.
///
/// Use this in tests to ensure cleanup happens even on panic.
pub struct PoolCleanup {
    pub pool: String,
    pub dev: String,
}

impl Drop for PoolCleanup {
    fn drop(&mut self) {
        destroy_pool(&self.pool);
        detach_loop_device(&self.dev);
    }
}

// ---------------------------------------------------------------------------
// ZFS assertions
// ---------------------------------------------------------------------------

/// Assert that a ZFS pool exists.
pub fn assert_pool_exists(pool: &str) {
    let output = Command::new("zpool")
        .args(["list", "-H", pool])
        .output()
        .expect("failed to run zpool");
    assert!(output.status.success(), "expected pool '{pool}' to exist");
}

/// Assert that a ZFS dataset (filesystem, zvol, etc.) exists.
pub fn assert_dataset_exists(dataset: &str) {
    let output = Command::new("zfs")
        .args(["list", "-H", dataset])
        .output()
        .expect("failed to run zfs");
    assert!(
        output.status.success(),
        "expected dataset '{dataset}' to exist"
    );
}

/// Assert that a ZFS dataset does NOT exist.
pub fn assert_dataset_absent(dataset: &str) {
    let output = Command::new("zfs")
        .args(["list", "-H", dataset])
        .output()
        .expect("failed to run zfs");
    assert!(
        !output.status.success(),
        "expected dataset '{dataset}' to NOT exist"
    );
}

/// Assert that a ZFS snapshot exists.
pub fn assert_snapshot_exists(snapshot: &str) {
    let output = Command::new("zfs")
        .args(["list", "-t", "snapshot", "-H", snapshot])
        .output()
        .expect("failed to run zfs");
    assert!(
        output.status.success(),
        "expected snapshot '{snapshot}' to exist"
    );
}

/// Assert that a ZFS snapshot does NOT exist.
pub fn assert_snapshot_absent(snapshot: &str) {
    let output = Command::new("zfs")
        .args(["list", "-t", "snapshot", "-H", snapshot])
        .output()
        .expect("failed to run zfs");
    assert!(
        !output.status.success(),
        "expected snapshot '{snapshot}' to NOT exist"
    );
}

/// Assert that a ZFS zvol exists.
pub fn assert_zvol_exists(zvol: &str) {
    let output = Command::new("zfs")
        .args(["list", "-H", zvol])
        .output()
        .expect("failed to run zfs");
    assert!(output.status.success(), "expected zvol '{zvol}' to exist");
}

/// Assert that a ZFS zvol does NOT exist.
pub fn assert_zvol_absent(zvol: &str) {
    let output = Command::new("zfs")
        .args(["list", "-H", zvol])
        .output()
        .expect("failed to run zfs");
    assert!(
        !output.status.success(),
        "expected zvol '{zvol}' to NOT exist"
    );
}

// ---------------------------------------------------------------------------
// Zvol device helpers
// ---------------------------------------------------------------------------

/// Wait for a ZFS zvol device node to appear, up to ~5 seconds.
///
/// ZFS creates /dev/zvol/ symlinks asynchronously after zvol creation.
/// Tests that need to mount a zvol should call this first.
pub fn wait_for_zvol_device(device_path: &str) -> bool {
    for _ in 0..50 {
        if Path::new(device_path).exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Mount a zvol read-write, run a closure with the mount path, then unmount.
///
/// Returns the closure's result. Panics if mount or umount fails.
pub fn with_mounted_zvol<F, T>(zvol_device: &str, f: F) -> T
where
    F: FnOnce(&Path) -> T,
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

/// Get the ZFS volsize property in bytes.
pub fn get_zvol_size_bytes(zvol: &str) -> u64 {
    let output = Command::new("zfs")
        .args(["get", "-Hp", "-o", "value", "volsize", zvol])
        .output()
        .expect("failed to run zfs get volsize");
    assert!(
        output.status.success(),
        "zfs get volsize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .expect("failed to parse volsize")
}

// ---------------------------------------------------------------------------
// Firecracker / kernel helpers
// ---------------------------------------------------------------------------

/// Check that Firecracker prerequisites are met: binary in PATH and /dev/kvm.
///
/// Returns false (with an eprintln message) if anything is missing,
/// allowing tests to skip gracefully.
pub fn firecracker_available() -> bool {
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

/// Check that Docker is available (needed for building ubuntu-slim image).
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Path where the downloaded Firecracker kernel is cached.
const KERNEL_CACHE_PATH: &str = "/tmp/ember-test-vmlinux";

/// URL for the Firecracker CI kernel (x86_64).
const KERNEL_URL: &str =
    "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/x86_64/vmlinux-6.1.102";

/// Get a bootable kernel for Firecracker tests.
///
/// Resolution order:
/// 1. `EMBER_TEST_KERNEL` env var (explicit override)
/// 2. Cached download at `/tmp/ember-test-vmlinux`
/// 3. Fresh download from the Firecracker CI S3 bucket
///
/// Returns `None` if download fails (test should skip gracefully).
pub fn ensure_kernel() -> Option<PathBuf> {
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

    // Download to a unique temp file, then rename atomically to avoid
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

/// Create a dummy kernel file (for tests that don't boot a real VM).
///
/// Tests that use `--no-start` still need a kernel path argument.
pub fn create_dummy_kernel(dir: &Path) -> PathBuf {
    let kernel = dir.join("vmlinux-dummy");
    std::fs::write(&kernel, b"not a real kernel").unwrap();
    kernel
}

// ---------------------------------------------------------------------------
// SSH helpers
// ---------------------------------------------------------------------------

/// Find the invoking user's SSH private key.
///
/// Checks `~/.ssh/` for common key types (ed25519, ecdsa, rsa).
/// Handles sudo by resolving the original user's home directory via
/// the `SUDO_USER` env var.
pub fn ssh_private_key_path() -> Option<PathBuf> {
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

/// Run a command on a guest via SSH.
///
/// Returns Ok(stdout) on success, Err(stderr) on failure.
/// Uses strict options to avoid interactive prompts.
pub fn ssh_exec(guest_ip: &str, key_path: &Path, command: &str) -> Result<String, String> {
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

/// Wait for SSH to become available on a guest, with exponential backoff.
///
/// Tries up to 16 times with increasing delays (total ~60s).
/// Returns true if SSH connected, false on timeout.
pub fn wait_for_ssh(guest_ip: &str, key_path: &Path) -> bool {
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
// Composite setup helpers
// ---------------------------------------------------------------------------
//
// These combine the primitives above into common test setup patterns.
// They call `super::ember()` — the cross-platform CLI helper in
// common/mod.rs — to drive the ember binary.

/// Set up a ZFS pool and run `ember init`.
///
/// Creates a 512 MB loopback device, attaches it, creates a ZFS pool,
/// and runs `ember init`. Returns (pool_name, state_dir, cleanup_guard).
pub fn setup_pool_and_init(
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

    let output = super::ember(&[
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

    (pool, state_dir, cleanup)
}

/// Set up a ZFS pool, run `ember init`, and pull the alpine image.
///
/// Returns (pool_name, state_dir, cleanup_guard).
pub fn setup_pool_init_and_pull(
    test_name: &str,
    tmp: &tempfile::TempDir,
) -> (String, PathBuf, PoolCleanup) {
    let (pool, state_dir, cleanup) = setup_pool_and_init(test_name, tmp);

    let output = super::ember(&[
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

/// Set up a ZFS pool, run `ember init`, pull alpine, and create a stopped VM.
///
/// Uses the default disk size. Returns (pool_name, state_dir, cleanup_guard).
pub fn setup_pool_and_vm(
    test_name: &str,
    vm_name: &str,
    tmp: &tempfile::TempDir,
) -> (String, PathBuf, PoolCleanup) {
    let (pool, state_dir, cleanup) = setup_pool_init_and_pull(test_name, tmp);
    let kernel = create_dummy_kernel(tmp.path());

    let output = super::ember(&[
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

/// Set up a ZFS pool, run `ember init`, pull alpine, and create a stopped VM
/// with a specific disk size.
///
/// Returns (pool_name, state_dir, cleanup_guard).
pub fn setup_pool_and_vm_with_disk(
    test_name: &str,
    vm_name: &str,
    disk_size: &str,
    tmp: &tempfile::TempDir,
) -> (String, PathBuf, PoolCleanup) {
    let (pool, state_dir, cleanup) = setup_pool_init_and_pull(test_name, tmp);
    let kernel = create_dummy_kernel(tmp.path());

    let output = super::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "vm",
        "create",
        vm_name,
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--disk-size",
        disk_size,
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

/// Set up a ZFS pool (8 GB), run `ember init`, and build the ubuntu-slim image.
///
/// The ubuntu-slim image includes systemd, sshd, and networking tools —
/// everything needed for SSH and internet connectivity tests.
/// Requires Docker for the image build step.
pub fn setup_pool_init_and_build_ubuntu(
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

    let output = super::ember(&[
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
    let output = super::ember(&[
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

/// Stop and delete a VM (best-effort cleanup).
///
/// Uses `--force` to handle any state. Ignores errors since this is
/// typically called during test teardown.
pub fn stop_and_delete_vm(state_dir: &Path, vm_name: &str) {
    let state = state_dir.to_str().unwrap();
    let _ = super::ember(&["--state-dir", state, "vm", "stop", vm_name, "--force"]);
    let _ = super::ember(&["--state-dir", state, "vm", "delete", vm_name, "--force"]);
}
