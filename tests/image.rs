//! Integration tests for `ember image pull`, `image list`, and `image delete`.
//!
//! Cross-platform tests use `TestEnv` to abstract platform setup.
//! Platform-specific storage checks (ZFS zvol on Linux, .img file on macOS)
//! are gated with `#[cfg(target_os)]`.
//!
//! To run:
//!   ./run-integration-tests.sh image

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Cross-platform tests
// ---------------------------------------------------------------------------

/// `ember image pull` downloads an image and reports success.
#[test]
#[ignore]
fn pull_creates_image() {
    let env = common::TestEnv::init("imgpull");

    let output = common::ember(&["--state-dir", env.state(), "image", "pull", "alpine:latest"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "image pull failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("pulled successfully"),
        "expected success message in stdout: {stdout}"
    );

    // Platform-specific storage verification.
    #[cfg(target_os = "linux")]
    {
        let zvol = format!("{}/ember/images/library-alpine-latest", env.pool);
        common::linux::assert_dataset_exists(&zvol);
        common::linux::assert_snapshot_exists(&format!("{zvol}@base"));
    }

    #[cfg(target_os = "macos")]
    {
        let img_path = env.state_dir.join("images/data/library-alpine-latest.img");
        assert!(
            img_path.exists(),
            "expected image file at {}",
            img_path.display()
        );
        let metadata = std::fs::metadata(&img_path).unwrap();
        assert!(
            metadata.len() >= 10 * 1024 * 1024,
            "image file too small: {} bytes",
            metadata.len()
        );
    }
}

/// `ember image list` shows pulled images in table and JSON formats.
#[test]
#[ignore]
fn list_shows_pulled_image() {
    let env = common::TestEnv::with_image("imglist");

    // Table output.
    let list = common::ember(&["--state-dir", env.state(), "image", "list"]);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list.status.success(),
        "image list failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(
        stdout.contains("library-alpine-latest"),
        "expected local name in table output: {stdout}"
    );
    assert!(
        stdout.contains("docker.io/library/alpine:latest"),
        "expected full reference in table output: {stdout}"
    );

    // JSON output.
    let json_list = common::ember(&[
        "--state-dir",
        env.state(),
        "image",
        "list",
        "--format",
        "json",
    ]);
    let json_stdout = String::from_utf8_lossy(&json_list.stdout);
    assert!(json_list.status.success());

    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {json_stdout}"));
    let images = parsed["images"]
        .as_array()
        .expect("expected 'images' array");
    assert_eq!(images.len(), 1, "expected one image");
    assert_eq!(images[0]["local_name"], "library-alpine-latest");
    assert_eq!(images[0]["reference"], "docker.io/library/alpine:latest");
}

/// `ember image delete` removes the image from registry and storage.
#[test]
#[ignore]
fn delete_removes_image() {
    let env = common::TestEnv::with_image("imgdel");

    // Platform-specific: verify storage exists before delete.
    #[cfg(target_os = "linux")]
    {
        let zvol = format!("{}/ember/images/library-alpine-latest", env.pool);
        common::linux::assert_dataset_exists(&zvol);
    }

    #[cfg(target_os = "macos")]
    {
        let img_path = env.state_dir.join("images/data/library-alpine-latest.img");
        assert!(img_path.exists());
    }

    // Delete.
    let del = common::ember(&[
        "--state-dir",
        env.state(),
        "image",
        "delete",
        "alpine:latest",
    ]);
    let stdout = String::from_utf8_lossy(&del.stdout);
    let stderr = String::from_utf8_lossy(&del.stderr);
    assert!(
        del.status.success(),
        "image delete failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Platform-specific: verify storage is gone.
    #[cfg(target_os = "linux")]
    {
        let zvol = format!("{}/ember/images/library-alpine-latest", env.pool);
        common::linux::assert_dataset_absent(&zvol);
    }

    #[cfg(target_os = "macos")]
    {
        let img_path = env.state_dir.join("images/data/library-alpine-latest.img");
        assert!(
            !img_path.exists(),
            "image file should have been deleted: {}",
            img_path.display()
        );
    }

    // Image list should be empty.
    let list = common::ember(&["--state-dir", env.state(), "image", "list"]);
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("No images found"),
        "expected empty list, got: {list_stdout}"
    );
}

/// Pulling the same image twice is idempotent.
#[test]
#[ignore]
fn pull_same_image_twice_is_idempotent() {
    let env = common::TestEnv::with_image("imgidempotent");

    // Second pull of the same image.
    let pull2 = common::ember(&["--state-dir", env.state(), "image", "pull", "alpine:latest"]);
    let stdout2 = String::from_utf8_lossy(&pull2.stdout);
    assert!(
        pull2.status.success(),
        "second pull failed: {}",
        String::from_utf8_lossy(&pull2.stderr)
    );
    assert!(
        stdout2.contains("already exists"),
        "expected 'already exists' on re-pull: {stdout2}"
    );
}
