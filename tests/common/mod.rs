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

/// Check that Docker is available (needed for building ubuntu-slim image).
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Stop and delete a VM (best-effort cleanup).
///
/// Uses `--force` to handle any state. Ignores errors since this is
/// typically called during test teardown.
pub fn stop_and_delete_vm(state_dir: &str, vm_name: &str) {
    let _ = ember(&["--state-dir", state_dir, "vm", "stop", vm_name, "--force"]);
    let _ = ember(&["--state-dir", state_dir, "vm", "delete", vm_name, "--force"]);
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

    /// Full running VM. Returns `None` if prerequisites are missing.
    ///
    /// Linux: needs Firecracker in PATH + `/dev/kvm` + bootable kernel.
    /// macOS: needs `ember-vz` binary + AVF-compatible kernel.
    ///
    /// Creates a VM with a real kernel, starts it, and waits briefly for
    /// boot. Tests using this should explicitly stop the VM when done.
    pub fn with_running_vm(test_name: &str, vm_name: &str) -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            if !linux::firecracker_available() {
                eprintln!("Skipping: firecracker not available");
                return None;
            }
            let kernel = linux::ensure_kernel()?;
            let env = Self::with_image(test_name);

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
                "--cpus",
                "1",
                "--memory",
                "128M",
                "--no-start",
            ]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
            );

            let output = ember(&["--state-dir", env.state(), "vm", "start", vm_name]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "vm start failed.\nstdout: {stdout}\nstderr: {stderr}"
            );

            Some(env)
        }

        #[cfg(target_os = "macos")]
        {
            let _ = macos::ember_vz_bin()?;
            let kernel = macos::ensure_kernel()?;
            let env = Self::with_image(test_name);

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
                "--cpus",
                "1",
                "--memory",
                "256M",
                "--no-start",
            ]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
            );

            let output = ember(&["--state-dir", env.state(), "vm", "start", vm_name]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "vm start failed.\nstdout: {stdout}\nstderr: {stderr}"
            );

            Some(env)
        }
    }

    /// Full running VM with SSH access (ubuntu-slim image with sshd).
    /// Returns `None` if prerequisites are missing.
    ///
    /// Linux: needs Firecracker + Docker + `/dev/kvm` + bootable kernel.
    /// macOS: needs ember-vz + Docker + AVF-compatible kernel.
    ///
    /// Builds `ubuntu-slim` via Docker, creates a VM, starts it, and waits
    /// for SSH to become available via `ember exec`. Tests using this should
    /// explicitly stop/delete the VM when done.
    pub fn with_running_ssh_vm(test_name: &str, vm_name: &str) -> Option<Self> {
        if !docker_available() {
            eprintln!("Skipping: docker not available (needed to build ubuntu-slim)");
            return None;
        }

        #[cfg(target_os = "linux")]
        {
            if !linux::firecracker_available() {
                eprintln!("Skipping: firecracker not available");
                return None;
            }
            let kernel = linux::ensure_kernel()?;

            // Ubuntu-slim needs a larger pool than alpine (8G).
            let tmp = tempfile::tempdir().unwrap();
            let (pool, state_dir, cleanup) =
                linux::setup_pool_init_and_build_ubuntu(test_name, &tmp);

            let env = TestEnv {
                state_dir,
                pool,
                _cleanup: Box::new(cleanup),
                _tmp: tmp,
            };

            let output = ember(&[
                "--state-dir",
                env.state(),
                "vm",
                "create",
                vm_name,
                "--image",
                "ubuntu-slim",
                "--kernel",
                kernel.to_str().unwrap(),
                "--cpus",
                "1",
                "--memory",
                "512M",
                "--no-start",
            ]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
            );

            let output = ember(&["--state-dir", env.state(), "vm", "start", vm_name]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "vm start failed.\nstdout: {stdout}\nstderr: {stderr}"
            );

            // Wait for SSH to be ready by retrying `ember exec` with a simple
            // command. Systemd + sshd can take 30-60s to come up.
            wait_for_ssh_via_exec(env.state(), vm_name);

            Some(env)
        }

        #[cfg(target_os = "macos")]
        {
            let _ = test_name; // Only used on Linux for ZFS pool naming.
            let _ = macos::ember_vz_bin()?;
            let kernel = macos::ensure_kernel()?;

            let tmp = tempfile::tempdir().unwrap();
            let state_dir = macos::setup_init(tmp.path());

            // Build ubuntu-slim image via Docker.
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

            let env = TestEnv {
                state_dir,
                _cleanup: Box::new(()),
                _tmp: tmp,
            };

            let output = ember(&[
                "--state-dir",
                env.state(),
                "vm",
                "create",
                vm_name,
                "--image",
                "ubuntu-slim",
                "--kernel",
                kernel.to_str().unwrap(),
                "--cpus",
                "1",
                "--memory",
                "256M",
                "--no-start",
            ]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
            );

            let output = ember(&["--state-dir", env.state(), "vm", "start", vm_name]);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "vm start failed.\nstdout: {stdout}\nstderr: {stderr}"
            );

            // Wait for SSH to be ready by retrying `ember exec` with a simple
            // command. Systemd + sshd can take 30-60s to come up.
            wait_for_ssh_via_exec(env.state(), vm_name);

            Some(env)
        }
    }
}

/// Wait for SSH to become available by retrying `ember exec <vm> -- true`.
///
/// Retries every 3 seconds for up to ~120 seconds. Panics if SSH never
/// becomes reachable (test should fail with a clear error).
fn wait_for_ssh_via_exec(state_dir: &str, vm_name: &str) {
    eprintln!("Waiting for SSH to become available on {vm_name}...");
    for attempt in 1..=40 {
        let output = ember(&["--state-dir", state_dir, "exec", vm_name, "--", "true"]);
        if output.status.success() {
            eprintln!("SSH ready on attempt {attempt}");
            return;
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    panic!("SSH not available on {vm_name} after 120 seconds");
}
