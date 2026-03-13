//! Integration tests for `ember vm resize` (Linux-only).
//!
//! These tests require:
#![cfg(target_os = "linux")]
//!
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

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Resize a stopped VM: verify zvol grows, ext4 expands, and metadata updates.
#[test]
#[ignore]
fn resize_stopped_vm_grows_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm_with_disk("vmresize", "resizevm", "1G", &tmp);
    let state = state_dir.to_str().unwrap();
    let vm_zvol = format!("{pool}/ember/vms/resizevm");
    let zvol_device = format!("/dev/zvol/{vm_zvol}");

    // Verify initial metadata shows 1 GiB.
    let inspect = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "resizevm",
        "--format",
        "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
        .expect("failed to parse inspect JSON");
    assert_eq!(
        json["disk_size_gib"], 1,
        "initial metadata should show 1 GiB"
    );

    // Verify initial ZFS volsize.
    let initial_bytes = common::linux::get_zvol_size_bytes(&vm_zvol);
    assert_eq!(
        initial_bytes,
        1 * 1024 * 1024 * 1024,
        "initial zvol should be 1 GiB"
    );

    // -- Resize to 2 GiB --
    let resize_output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "resizevm",
        "--disk-size",
        "2G",
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
    let new_bytes = common::linux::get_zvol_size_bytes(&vm_zvol);
    assert_eq!(
        new_bytes,
        2 * 1024 * 1024 * 1024,
        "zvol should be 2 GiB after resize"
    );

    // -- Verify metadata updated --
    let inspect2 = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "resizevm",
        "--format",
        "json",
    ]);
    assert!(inspect2.status.success());
    let json2: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect2.stdout))
        .expect("failed to parse inspect JSON after resize");
    assert_eq!(
        json2["disk_size_gib"], 2,
        "metadata should show 2 GiB after resize"
    );

    // -- Verify ext4 filesystem was expanded --
    // Mount the zvol and check that available space reflects the new size.
    assert!(
        common::linux::wait_for_zvol_device(&zvol_device),
        "zvol device {zvol_device} did not appear within timeout"
    );

    common::linux::with_mounted_zvol(&zvol_device, |mount| {
        // Use statvfs to check total filesystem size. After resize2fs,
        // the ext4 filesystem should be close to 2 GiB.
        let output = std::process::Command::new("df")
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
        common::linux::setup_pool_and_vm_with_disk("vmresizeshrink", "shrinkvm", "2G", &tmp);
    let state = state_dir.to_str().unwrap();

    // Try to shrink from 2 GiB to 1 GiB.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "shrinkvm",
        "--disk-size",
        "1G",
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
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "shrinkvm",
        "--disk-size",
        "2G",
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
    let pool = common::linux::test_pool("vmresizenovm");
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

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "nosuchvm",
        "--disk-size",
        "16G",
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
        common::linux::setup_pool_and_vm_with_disk("vmresizemulti", "multivm", "1G", &tmp);
    let state = state_dir.to_str().unwrap();
    let vm_zvol = format!("{pool}/ember/vms/multivm");

    // Resize 1 GiB → 2 GiB.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "multivm",
        "--disk-size",
        "2G",
    ]);
    assert!(
        output.status.success(),
        "first resize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        common::linux::get_zvol_size_bytes(&vm_zvol),
        2 * 1024 * 1024 * 1024
    );

    // Resize 2 GiB → 4 GiB.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "multivm",
        "--disk-size",
        "4G",
    ]);
    assert!(
        output.status.success(),
        "second resize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        common::linux::get_zvol_size_bytes(&vm_zvol),
        4 * 1024 * 1024 * 1024
    );

    // Verify metadata tracks the latest size.
    let inspect = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "multivm",
        "--format",
        "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
        .expect("failed to parse inspect JSON");
    assert_eq!(json["disk_size_gib"], 4, "metadata should show 4 GiB");
}
