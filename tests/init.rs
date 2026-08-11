//! Integration tests for `ember init`.
//!
//! Cross-platform tests use `TestEnv::init()` to abstract platform setup.
//! Platform-specific tests (ZFS verification on Linux, no-root check on macOS)
//! are gated with `#[cfg(target_os)]`.
//!
//! To run:
//!   ./run-integration-tests.sh init

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Cross-platform tests
// ---------------------------------------------------------------------------

/// `ember init` creates the expected directory structure.
#[test]
#[ignore]
fn init_creates_directory_structure() {
    let env = common::TestEnv::init("initdirs");

    for dir in &["vms", "kernels", "images", "network"] {
        let path = env.state_dir.join(dir);
        assert!(
            path.is_dir(),
            "expected directory to exist: {}",
            path.display()
        );
    }

    #[cfg(target_os = "macos")]
    assert!(
        env.state_dir.join("images/data").is_dir(),
        "macOS should have images/data/"
    );
}

/// `ember init` writes a valid config.json.
#[test]
#[ignore]
fn init_writes_config_json() {
    let env = common::TestEnv::init("initcfg");

    let config_path = env.state_dir.join("config.json");
    assert!(config_path.exists(), "config.json not found");

    let content = std::fs::read_to_string(&config_path).unwrap();
    let config: serde_json::Value =
        serde_json::from_str(&content).expect("config.json is not valid JSON");

    #[cfg(target_os = "macos")]
    {
        let stored = config["state_dir"].as_str().unwrap();
        assert_eq!(
            stored,
            env.state_dir.to_str().unwrap(),
            "state_dir in config.json doesn't match"
        );
    }

    #[cfg(target_os = "linux")]
    {
        assert_eq!(config["pool"], env.pool);
        assert_eq!(config["dataset"], "ember");
    }
}

/// Re-running `ember init` on an initialized state dir is rejected.
///
/// An existing config wins; the user must `ember deinit` first to
/// reconfigure. This guards against silently clobbering a live install.
#[test]
#[ignore]
fn init_rejects_reinit() {
    let env = common::TestEnv::init("initidem");

    // Run init again on the same state directory.
    #[cfg(target_os = "macos")]
    let output = common::ember(&["--state-dir", env.state(), "init"]);

    #[cfg(target_os = "linux")]
    let output = common::ember(&["--state-dir", env.state(), "init", "--pool", &env.pool]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "second init should be rejected.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("already initialized") && stderr.contains("deinit"),
        "expected an 'already initialized — run deinit' error.\nstderr: {stderr}"
    );

    // The existing install is left intact.
    assert!(env.state_dir.join("vms").is_dir());
    assert!(env.state_dir.join("images").is_dir());

    #[cfg(target_os = "linux")]
    {
        common::linux::assert_pool_exists(&env.pool);
        common::linux::assert_dataset_exists(&format!("{}/ember", env.pool));
        common::linux::assert_dataset_exists(&format!("{}/ember/images", env.pool));
        common::linux::assert_dataset_exists(&format!("{}/ember/vms", env.pool));
    }
}

// ---------------------------------------------------------------------------
// Linux-specific tests
// ---------------------------------------------------------------------------

/// Verify ZFS pool and datasets are created by `ember init`.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn init_creates_pool_and_datasets() {
    let env = common::TestEnv::init("initpool");

    common::linux::assert_pool_exists(&env.pool);
    common::linux::assert_dataset_exists(&format!("{}/ember", env.pool));
    common::linux::assert_dataset_exists(&format!("{}/ember/images", env.pool));
    common::linux::assert_dataset_exists(&format!("{}/ember/vms", env.pool));
}

/// `ember init` without `--device` fails when pool doesn't exist.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn init_fails_without_device_when_pool_missing() {
    let pool = common::linux::test_pool("nodevice");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");

    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--pool",
        &pool,
    ]);

    assert!(
        !output.status.success(),
        "expected init to fail without --device"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist") && stderr.contains("--device"),
        "expected helpful error about --device, got: {stderr}"
    );
}

/// `ember init --dataset` uses a custom dataset name.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn init_custom_dataset_name() {
    let pool = common::linux::test_pool("customds");
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path().join("state");
    let (loop_dev, img) = common::linux::create_loop_device(tmp.path());

    let _cleanup = common::linux::PoolCleanup {
        pool: pool.clone(),
        dev: loop_dev.clone(),
        backing_file: img,
        state_dir: state_dir.clone(),
    };

    let output = common::ember(&[
        "--state-dir",
        state_dir.to_str().unwrap(),
        "init",
        "--pool",
        &pool,
        "--device",
        &loop_dev,
        "--dataset",
        "mydata",
    ]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "init failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    common::linux::assert_dataset_exists(&format!("{pool}/mydata"));
    common::linux::assert_dataset_exists(&format!("{pool}/mydata/images"));
    common::linux::assert_dataset_exists(&format!("{pool}/mydata/vms"));

    let config_str = std::fs::read_to_string(state_dir.join("config.json")).unwrap();
    let config: serde_json::Value = serde_json::from_str(&config_str).unwrap();
    assert_eq!(config["dataset"], "mydata");
}

// ---------------------------------------------------------------------------
// macOS-specific tests
// ---------------------------------------------------------------------------

/// `ember init` does not require root on macOS.
#[cfg(target_os = "macos")]
#[test]
#[ignore]
fn init_works_without_root() {
    assert!(
        !nix::unistd::geteuid().is_root(),
        "this test should run as a non-root user"
    );

    let env = common::TestEnv::init("initnoroot");
    assert!(env.state_dir.join("config.json").exists());
    assert!(env.state_dir.join("vms").is_dir());
}
