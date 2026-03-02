//! Integration tests for `ember vm resize`.
//!
//! These tests require:
//! - Root privileges
//! - Working ZFS installation
//! - Network access (to pull OCI images from Docker Hub)
//! - `skopeo` installed
//! - `e2fsck` and `resize2fs` (e2fsprogs package)
//!
//! They are marked `#[ignore]` so `cargo test` skips them by default.
//!
//! To run:
//!   ./run-integration-tests.sh resize

use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// Shared helpers (same pattern as other integration tests)
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

    let dev = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .to_string();
    (dev, file)
}

fn detach_loop_device(dev: &str) {
    let _ = Command::new("losetup").args(["-d", dev]).status();
}

fn destroy_pool(pool: &str) {
    let _ = Command::new("zpool")
        .args(["destroy", "-f", pool])
        .status();
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

/// Set up a ZFS pool, run `ember init`, pull alpine, and create a VM.
/// Returns (pool_name, state_dir, cleanup_guard).
fn setup_pool_and_vm(
    test_name: &str,
    vm_name: &str,
    disk_size: u32,
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

    let disk_size_str = disk_size.to_string();

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
        "--disk-size",
        &disk_size_str,
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

/// Get the ZFS volsize property in bytes.
fn get_zvol_size_bytes(zvol: &str) -> u64 {
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

/// Resize a stopped VM: verify zvol grows, ext4 expands, and metadata updates.
#[test]
#[ignore]
fn resize_stopped_vm_grows_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        setup_pool_and_vm("vmresize", "resizevm", 1, &tmp);
    let state = state_dir.to_str().unwrap();
    let vm_zvol = format!("{pool}/ember/vms/resizevm");
    let zvol_device = format!("/dev/zvol/{vm_zvol}");

    // Verify initial metadata shows 1 GiB.
    let inspect = ember(&[
        "--state-dir", state,
        "vm", "inspect", "resizevm", "--format", "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
            .expect("failed to parse inspect JSON");
    assert_eq!(json["disk_size_gib"], 1, "initial metadata should show 1 GiB");

    // Verify initial ZFS volsize.
    let initial_bytes = get_zvol_size_bytes(&vm_zvol);
    assert_eq!(
        initial_bytes,
        1 * 1024 * 1024 * 1024,
        "initial zvol should be 1 GiB"
    );

    // -- Resize to 2 GiB --
    let resize_output = ember(&[
        "--state-dir", state,
        "vm", "resize", "resizevm",
        "--disk-size", "2",
    ]);
    let stdout = String::from_utf8_lossy(&resize_output.stdout);
    let stderr = String::from_utf8_lossy(&resize_output.stderr);
    assert!(
        resize_output.status.success(),
        "vm resize failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("resized"),
        "expected confirmation message: {stdout}"
    );

    // -- Verify ZFS volsize grew --
    let new_bytes = get_zvol_size_bytes(&vm_zvol);
    assert_eq!(
        new_bytes,
        2 * 1024 * 1024 * 1024,
        "zvol should be 2 GiB after resize"
    );

    // -- Verify metadata updated --
    let inspect2 = ember(&[
        "--state-dir", state,
        "vm", "inspect", "resizevm", "--format", "json",
    ]);
    assert!(inspect2.status.success());
    let json2: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&inspect2.stdout))
            .expect("failed to parse inspect JSON after resize");
    assert_eq!(
        json2["disk_size_gib"], 2,
        "metadata should show 2 GiB after resize"
    );

    // -- Verify ext4 filesystem was expanded --
    // Mount the zvol and check that available space reflects the new size.
    assert!(
        wait_for_zvol_device(&zvol_device),
        "zvol device {zvol_device} did not appear within timeout"
    );

    with_mounted_zvol(&zvol_device, |mount| {
        // Use statvfs to check total filesystem size. After resize2fs,
        // the ext4 filesystem should be close to 2 GiB.
        let output = Command::new("df")
            .args(["--output=size", "-B1"])
            .arg(mount)
            .output()
            .expect("failed to run df");
        assert!(output.status.success(), "df failed");

        let df_output = String::from_utf8_lossy(&output.stdout);
        let size_line = df_output.lines().nth(1).expect("expected df output line");
        let fs_bytes: u64 = size_line.trim().parse().expect("failed to parse df size");

        // ext4 has some overhead, so the filesystem won't be exactly 2 GiB.
        // Check it's at least 1.8 GiB (allowing ~10% overhead).
        let min_expected = (1.8 * 1024.0 * 1024.0 * 1024.0) as u64;
        assert!(
            fs_bytes >= min_expected,
            "ext4 filesystem should be ~2 GiB after resize, got {} bytes ({:.2} GiB)",
            fs_bytes,
            fs_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        );
    });

    eprintln!("Resize grow test passed: zvol, ext4, and metadata all updated.");
}

/// Shrinking (or same size) should be rejected.
#[test]
#[ignore]
fn resize_shrink_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) =
        setup_pool_and_vm("vmresizeshrink", "shrinkvm", 2, &tmp);
    let state = state_dir.to_str().unwrap();

    // Try to shrink from 2 GiB to 1 GiB.
    let output = ember(&[
        "--state-dir", state,
        "vm", "resize", "shrinkvm",
        "--disk-size", "1",
    ]);
    assert!(
        !output.status.success(),
        "expected resize to smaller size to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be larger"),
        "expected 'must be larger' error: {stderr}"
    );

    // Try same size.
    let output = ember(&[
        "--state-dir", state,
        "vm", "resize", "shrinkvm",
        "--disk-size", "2",
    ]);
    assert!(
        !output.status.success(),
        "expected resize to same size to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must be larger"),
        "expected 'must be larger' error: {stderr}"
    );
}

/// Resizing a nonexistent VM should fail.
#[test]
#[ignore]
fn resize_nonexistent_vm_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = test_pool("vmresizenovm");
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

    let output = ember(&[
        "--state-dir", state,
        "vm", "resize", "nosuchvm",
        "--disk-size", "16",
    ]);
    assert!(
        !output.status.success(),
        "expected resize of nonexistent VM to fail"
    );
}

/// Multiple sequential resizes should all succeed.
#[test]
#[ignore]
fn resize_multiple_grows() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        setup_pool_and_vm("vmresizemulti", "multivm", 1, &tmp);
    let state = state_dir.to_str().unwrap();
    let vm_zvol = format!("{pool}/ember/vms/multivm");

    // Resize 1 GiB → 2 GiB.
    let output = ember(&[
        "--state-dir", state,
        "vm", "resize", "multivm",
        "--disk-size", "2",
    ]);
    assert!(
        output.status.success(),
        "first resize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(get_zvol_size_bytes(&vm_zvol), 2 * 1024 * 1024 * 1024);

    // Resize 2 GiB → 4 GiB.
    let output = ember(&[
        "--state-dir", state,
        "vm", "resize", "multivm",
        "--disk-size", "4",
    ]);
    assert!(
        output.status.success(),
        "second resize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(get_zvol_size_bytes(&vm_zvol), 4 * 1024 * 1024 * 1024);

    // Verify metadata tracks the latest size.
    let inspect = ember(&[
        "--state-dir", state,
        "vm", "inspect", "multivm", "--format", "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
            .expect("failed to parse inspect JSON");
    assert_eq!(json["disk_size_gib"], 4, "metadata should show 4 GiB");
}
