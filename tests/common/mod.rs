//! Cross-platform test helpers and TestEnv abstraction.
//!
//! This module provides the shared test infrastructure:
//! - `ember_bin()` / `ember()`: locate and run the ember CLI binary
//! - `TestEnv`: encapsulates platform-specific setup (ZFS on Linux, APFS temp
//!   dirs on macOS) behind a uniform interface with `init()`, `with_image()`,
//!   and `with_vm()` constructors
//!
//! Platform-specific helpers live in submodules:
//! - `linux`: ZFS pool/loopback, Firecracker, SSH helpers
//! - `macos`: ember-vz, e2fsprogs, APFS clone, manual VM setup helpers
//!
//! Not all test files use all helpers — dead_code warnings are suppressed
//! via `#[allow(dead_code)]` on the `mod common;` declaration in each test file.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

// Transitional re-exports: existing test files reference these as `common::X`.
// They will be updated to `common::macos::X` in the next task, then these
// re-exports can be removed.
#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub use macos::{
    create_test_rootfs, ember_vz_bin, ensure_kernel, find_e2fsprogs_tool, read_mac_from_pipe,
    setup_init, spawn_ember_vz, wait_for_exit,
};

use std::path::PathBuf;
use std::process::Command;

// ---------------------------------------------------------------------------
// Ember CLI helpers
// ---------------------------------------------------------------------------

/// Path to the ember binary built by cargo.
pub fn ember_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("ember");
    path
}

/// Run ember with the given args, returning the Output.
pub fn ember(args: &[&str]) -> std::process::Output {
    Command::new(ember_bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute ember: {e}"))
}

// ---------------------------------------------------------------------------
// TestEnv — cross-platform test environment
// ---------------------------------------------------------------------------

/// Encapsulates platform-specific test setup behind a uniform interface.
///
/// On Linux: creates a loopback device + ZFS pool, cleaned up on drop.
/// On macOS: uses a temp directory on APFS, no special cleanup needed.
///
/// Constructors run real `ember` CLI commands (black-box testing) and
/// panic on failure. Tests that need finer control can use the
/// platform-specific helpers in `linux::` or `macos::` directly.
pub struct TestEnv {
    pub state_dir: PathBuf,

    /// ZFS pool name (Linux only). Useful for ZFS-specific assertions.
    #[cfg(target_os = "linux")]
    pub pool: String,

    /// Cleanup guard: PoolCleanup on Linux (destroys pool + detaches loop
    /// device on drop), no-op on macOS.
    _cleanup: Box<dyn std::any::Any>,

    /// Temp directory that holds the state dir. Kept alive for the
    /// lifetime of the TestEnv; cleaned up on drop.
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    /// Returns the state directory path as a string slice.
    ///
    /// Convenience for passing to `ember(&["--state-dir", env.state(), ...])`.
    pub fn state(&self) -> &str {
        self.state_dir.to_str().unwrap()
    }

    /// Just `ember init`.
    ///
    /// Linux: creates loopback device → ZFS pool → `ember init --pool --device`.
    /// macOS: `ember init` (no extra args, temp directory suffices).
    pub fn init(test_name: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();

        #[cfg(target_os = "linux")]
        {
            let (pool, state_dir, cleanup) = linux::setup_pool_and_init(test_name, &tmp);
            TestEnv {
                state_dir,
                pool,
                _cleanup: Box::new(cleanup),
                _tmp: tmp,
            }
        }

        #[cfg(target_os = "macos")]
        {
            let _ = test_name; // Only used on Linux for ZFS pool naming.
            let state_dir = macos::setup_init(tmp.path());
            TestEnv {
                state_dir,
                _cleanup: Box::new(()),
                _tmp: tmp,
            }
        }
    }

    /// `ember init` + `ember image pull alpine:latest`.
    pub fn with_image(test_name: &str) -> Self {
        let env = Self::init(test_name);

        let output = ember(&["--state-dir", env.state(), "image", "pull", "alpine:latest"]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "image pull failed.\nstdout: {stdout}\nstderr: {stderr}"
        );

        env
    }

    /// `ember init` + image pull + `ember vm create --no-start`.
    ///
    /// Creates a stopped VM with a dummy kernel (not bootable).
    /// Suitable for snapshot, resize, fork, and metadata tests.
    pub fn with_vm(test_name: &str, vm_name: &str) -> Self {
        let env = Self::with_image(test_name);

        // Create a dummy kernel file — vm create validates the path exists
        // but --no-start doesn't actually boot it.
        let kernel = env._tmp.path().join("vmlinux-dummy");
        std::fs::write(&kernel, b"not a real kernel").unwrap();

        let output = ember(&[
            "--state-dir",
            env.state(),
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

        env
    }
}
