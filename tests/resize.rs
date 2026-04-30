//! Integration tests for `ember vm resize`.
//!
//! Cross-platform tests use `TestEnv::with_vm()` to abstract platform setup.
//! Platform-specific disk verification (ZFS volsize on Linux, file size +
//! dumpe2fs on macOS) is gated with `#[cfg(target_os)]`.
//!
//! To run:
//!   ./run-integration-tests.sh resize

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Cross-platform tests
// ---------------------------------------------------------------------------

/// Shrinking (or same size) should be rejected.
#[test]
#[ignore]
fn resize_shrink_fails() {
    let env = common::TestEnv::with_vm("resizeshrink", "shrinkvm");
    let state = env.state();

    // Inspect to get the current disk size.
    let inspect = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "shrinkvm",
        "--format",
        "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
        .expect("failed to parse inspect JSON");
    let current_gib = json["disk_size_gib"].as_u64().unwrap();

    // Try to shrink (half the current size).
    let smaller = format!("{}G", current_gib / 2);
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "shrinkvm",
        "--disk-size",
        &smaller,
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
    let same = format!("{current_gib}G");
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "shrinkvm",
        "--disk-size",
        &same,
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

/// Multiple sequential resizes should all succeed.
#[test]
#[ignore]
fn resize_multiple_grows() {
    let env = common::TestEnv::with_vm("resizemulti", "multivm");
    let state = env.state();

    // Inspect to get the current disk size, then grow from there.
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
    let base_gib = json["disk_size_gib"].as_u64().unwrap();

    // First grow: base → base + 2.
    let size1 = base_gib + 2;
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "multivm",
        "--disk-size",
        &format!("{size1}G"),
    ]);
    assert!(
        output.status.success(),
        "first resize failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Second grow: base + 2 → base + 4.
    let size2 = base_gib + 4;
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "multivm",
        "--disk-size",
        &format!("{size2}G"),
    ]);
    assert!(
        output.status.success(),
        "second resize failed: {}",
        String::from_utf8_lossy(&output.stderr)
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
    assert_eq!(
        json["disk_size_gib"], size2,
        "metadata should show {size2} GiB"
    );

    // Platform-specific size verification.
    #[cfg(target_os = "linux")]
    assert_eq!(
        common::linux::get_zvol_size_bytes(&format!("{}/ember/vms/multivm", env.pool)),
        size2 * 1024 * 1024 * 1024
    );

    #[cfg(target_os = "macos")]
    {
        let rootfs = env.state_dir.join("vms").join("multivm").join("rootfs.img");
        assert_eq!(
            std::fs::metadata(&rootfs).unwrap().len(),
            size2 * 1024 * 1024 * 1024
        );
    }
}

/// Resizing a nonexistent VM should fail.
#[test]
#[ignore]
fn resize_nonexistent_vm_fails() {
    let env = common::TestEnv::init("resizenovm");
    let state = env.state();

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

// ---------------------------------------------------------------------------
// Linux-specific tests
// ---------------------------------------------------------------------------

/// Resize a stopped VM: verify zvol grows, ext4 expands, and metadata updates.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn resize_grows_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        common::linux::setup_pool_and_vm_with_disk("vmresize", "resizevm", "1G", &tmp);
    let state = state_dir.to_str().unwrap();
    let vm_zvol = format!("{pool}/ember/vms/resizevm");
    let zvol_device = format!("/dev/zvol/{vm_zvol}");

    // Verify initial ZFS volsize.
    assert_eq!(
        common::linux::get_zvol_size_bytes(&vm_zvol),
        1024 * 1024 * 1024,
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
    assert_eq!(
        common::linux::get_zvol_size_bytes(&vm_zvol),
        2 * 1024 * 1024 * 1024,
        "zvol should be 2 GiB after resize"
    );

    // -- Verify metadata updated --
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
        json["disk_size_gib"], 2,
        "metadata should show 2 GiB after resize"
    );

    // -- Verify ext4 filesystem was expanded --
    assert!(
        common::linux::wait_for_zvol_device(&zvol_device),
        "zvol device {zvol_device} did not appear within timeout"
    );

    common::linux::with_mounted_zvol(&zvol_device, |mount| {
        let output = std::process::Command::new("df")
            .args(["--output=size", "-B1"])
            .arg(mount)
            .output()
            .expect("failed to run df");
        assert!(output.status.success(), "df failed");

        let df_output = String::from_utf8_lossy(&output.stdout);
        let size_line = df_output.lines().nth(1).expect("expected df output line");
        let fs_bytes: u64 = size_line.trim().parse().expect("failed to parse df size");

        let min_expected = (1.8 * 1024.0 * 1024.0 * 1024.0) as u64;
        assert!(
            fs_bytes >= min_expected,
            "ext4 filesystem should be ~2 GiB after resize, got {} bytes ({:.2} GiB)",
            fs_bytes,
            fs_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
        );
    });
}

// ---------------------------------------------------------------------------
// macOS-specific tests
// ---------------------------------------------------------------------------

/// Resize a stopped VM: verify .img file grows, ext4 expands, metadata updates.
///
/// Uses manual VM setup (bypasses `ember vm create`) to control initial
/// disk size precisely for file-size assertions.
#[cfg(target_os = "macos")]
#[test]
#[ignore]
fn resize_grows_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = common::macos::setup_with_vm(tmp.path(), "resize", "resizevm");
    let state = state_dir.to_str().unwrap();
    let rootfs = state_dir.join("vms").join("resizevm").join("rootfs.img");

    // Initial file size should be 64MB (from create_test_image).
    let initial_size = std::fs::metadata(&rootfs).unwrap().len();
    assert_eq!(
        initial_size,
        64 * 1024 * 1024,
        "initial image should be 64MB"
    );

    // -- Resize to 2 GiB --
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "resize",
        "resizevm",
        "--disk-size",
        "2G",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm resize failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("resized"),
        "expected confirmation message: {stdout}"
    );

    // -- Verify file size grew to 2 GiB --
    let new_size = std::fs::metadata(&rootfs).unwrap().len();
    assert_eq!(
        new_size,
        2 * 1024 * 1024 * 1024,
        "rootfs.img should be 2 GiB after resize, got {new_size} bytes"
    );

    // -- Verify metadata updated --
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
        json["disk_size_gib"], 2,
        "metadata should show 2 GiB after resize"
    );

    // -- Verify ext4 filesystem was expanded --
    let dumpe2fs = common::macos::find_e2fsprogs_tool("dumpe2fs");
    let output = std::process::Command::new(&dumpe2fs)
        .arg("-h")
        .arg(&rootfs)
        .output()
        .unwrap_or_else(|_| panic!("failed to run {dumpe2fs} — is e2fsprogs installed?"));
    assert!(
        output.status.success(),
        "dumpe2fs failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dump_stdout = String::from_utf8_lossy(&output.stdout);
    let block_count: u64 = common::macos::parse_dumpe2fs_value(&dump_stdout, "Block count");
    let block_size: u64 = common::macos::parse_dumpe2fs_value(&dump_stdout, "Block size");
    let fs_bytes = block_count * block_size;

    let min_expected = (1.8 * 1024.0 * 1024.0 * 1024.0) as u64;
    assert!(
        fs_bytes >= min_expected,
        "ext4 filesystem should be ~2 GiB after resize, got {fs_bytes} bytes ({:.2} GiB)",
        fs_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    );
}
