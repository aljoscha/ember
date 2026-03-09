//! Integration tests for the `ember-vz` Swift helper (Phase 1).
//!
//! These tests verify that ember-vz can boot a Linux VM using Apple
//! Virtualization Framework, produce serial console output, and configure
//! a vmnet network device.
//!
//! Requirements:
//! - macOS 13+ with Virtualization.framework
//! - `ember-vz` binary built (`swift build` in ember-vz/)
//! - Homebrew `e2fsprogs` (for mkfs.ext4)
//! - Network access (to download kernel on first run)
//! - No root required
//!
//! The kernel is resolved in order:
//! 1. `EMBER_TEST_KERNEL` env var (explicit override)
//! 2. Cached at `/tmp/ember-test-vmlinux`
//! 3. Downloaded from Firecracker CI (architecture-matched)
//!
//! To run:
//!   ./run-integration-tests.sh macos_ember_vz
#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Cache location for the downloaded test kernel.
const KERNEL_CACHE_PATH: &str = "/tmp/ember-test-vmlinux";

/// How long to wait for "vm started" on stderr before giving up.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for graceful shutdown after SIGTERM before sending SIGKILL.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Locate the `ember-vz` Swift helper binary.
///
/// Resolution order:
/// 1. `EMBER_VZ` env var
/// 2. `ember-vz/.build/release/ember-vz` (relative to project root)
/// 3. `ember-vz/.build/debug/ember-vz`
fn ember_vz_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("EMBER_VZ") {
        let path = PathBuf::from(&p);
        assert!(path.exists(), "EMBER_VZ={p} does not exist");
        return Some(path);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for subdir in ["release", "debug"] {
        let path = manifest_dir.join(format!("ember-vz/.build/{subdir}/ember-vz"));
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Get a bootable kernel for AVF tests.
///
/// Resolution order:
/// 1. `EMBER_TEST_KERNEL` env var (explicit override)
/// 2. Cached download at `/tmp/ember-test-vmlinux`
/// 3. Fresh download from Firecracker CI S3 bucket (architecture-matched)
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

    // Download to a temp file, then rename atomically.
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        _ => "x86_64",
    };
    let url = format!(
        "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/{arch}/vmlinux-6.1.102"
    );

    let tmp = PathBuf::from(format!("{KERNEL_CACHE_PATH}.{}", std::process::id()));
    eprintln!("Downloading kernel from {url}...");
    let status = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&tmp)
        .arg(&url)
        .status();

    match status {
        Ok(s) if s.success() => {
            let _ = std::fs::rename(&tmp, &cache);
            eprintln!("Kernel cached at {KERNEL_CACHE_PATH}");
            Some(cache)
        }
        _ => {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("Failed to download kernel — skipping");
            None
        }
    }
}

/// Find an e2fsprogs tool by checking Homebrew paths before falling back to PATH.
fn find_e2fsprogs_tool(name: &str) -> String {
    let arm_path = format!("/opt/homebrew/opt/e2fsprogs/sbin/{name}");
    if Path::new(&arm_path).exists() {
        return arm_path;
    }
    let intel_path = format!("/usr/local/opt/e2fsprogs/sbin/{name}");
    if Path::new(&intel_path).exists() {
        return intel_path;
    }
    name.to_string()
}

/// Create a minimal ext4 rootfs image file.
fn create_test_rootfs(dir: &Path, size_mb: u64) -> PathBuf {
    let img = dir.join("rootfs.img");

    let status = Command::new("truncate")
        .args(["-s", &format!("{size_mb}M")])
        .arg(&img)
        .status()
        .expect("failed to run truncate");
    assert!(status.success(), "truncate failed");

    let mkfs = find_e2fsprogs_tool("mkfs.ext4");
    let output = Command::new(&mkfs)
        .args(["-F", "-q"])
        .arg(&img)
        .output()
        .unwrap_or_else(|_| {
            panic!("failed to run {mkfs} — is e2fsprogs installed? (brew install e2fsprogs)")
        });
    assert!(
        output.status.success(),
        "mkfs.ext4 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    img
}

/// Send SIGTERM to a process.
fn send_sigterm(pid: u32) {
    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
}

/// Wait for a child process to exit within `timeout`, sending SIGKILL as fallback.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {
                if start.elapsed() > timeout {
                    eprintln!("Timeout waiting for exit — sending SIGKILL");
                    let _ = child.kill();
                    return child.wait().expect("failed to wait after kill");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("error waiting for process: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Boot a Linux VM via ember-vz, verify serial output, and verify network.
///
/// This test:
/// 1. Spawns `ember-vz start` with a Linux kernel and minimal ext4 rootfs
/// 2. Waits for the VM to boot (watches for "vm started" on stderr)
/// 3. Verifies the serial console log contains kernel boot messages
/// 4. Verifies a vmnet MAC address was assigned (network configured)
/// 5. Sends SIGTERM for graceful shutdown and verifies clean exit
///
/// The rootfs is an empty ext4 image — the kernel will boot and eventually
/// panic when no /sbin/init is found, but the serial output up to that point
/// is sufficient to verify serial console and network device configuration.
#[test]
#[ignore]
fn ember_vz_boot_serial_and_network() {
    // --- Prerequisites ---

    let ember_vz = match ember_vz_bin() {
        Some(p) => {
            eprintln!("Using ember-vz: {}", p.display());
            p
        }
        None => {
            eprintln!("Skipping: ember-vz not found (run 'swift build' in ember-vz/)");
            return;
        }
    };

    let kernel = match ensure_kernel() {
        Some(k) => {
            eprintln!("Using kernel: {}", k.display());
            k
        }
        None => {
            eprintln!("Skipping: no kernel available (set EMBER_TEST_KERNEL or allow download)");
            return;
        }
    };

    // --- Setup ---

    let tmp = tempfile::tempdir().unwrap();
    let rootfs = create_test_rootfs(tmp.path(), 64);
    let serial_log = tmp.path().join("console.log");

    // --- Spawn ember-vz ---

    eprintln!("Starting VM...");
    let mut child = Command::new(&ember_vz)
        .args([
            "start",
            "--kernel",
            kernel.to_str().unwrap(),
            "--disk",
            rootfs.to_str().unwrap(),
            "--cpus",
            "1",
            "--memory",
            "256",
            "--boot-args",
            "console=hvc0 root=/dev/vda rw",
            "--serial-log",
            serial_log.to_str().unwrap(),
        ])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn ember-vz: {e}"));

    let child_pid = child.id();

    // Read stderr in a background thread so it doesn't block the main thread.
    let stderr_handle = child.stderr.take().unwrap();
    let stderr_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lines_for_reader = stderr_lines.clone();
    let reader_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stderr_handle);
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("  [ember-vz] {line}");
            lines_for_reader.lock().unwrap().push(line);
        }
    });

    // --- Wait for boot ---

    let boot_start = Instant::now();
    let mut booted = false;
    while boot_start.elapsed() < BOOT_TIMEOUT {
        {
            let lines = stderr_lines.lock().unwrap();

            // Check for successful boot.
            if lines.iter().any(|l| l.contains("vm started")) {
                booted = true;
                break;
            }

            // Check for early error.
            if lines
                .iter()
                .any(|l| l.starts_with("error:") || l.contains("vm failed to start"))
            {
                let all = lines.join("\n");
                // Still need to clean up the process before panicking.
                drop(lines);
                send_sigterm(child_pid);
                let _ = wait_with_timeout(&mut child, STOP_TIMEOUT);
                reader_thread.join().unwrap();
                panic!("ember-vz failed to start:\n{all}");
            }
        }

        // Check if the process exited unexpectedly.
        if let Ok(Some(status)) = child.try_wait() {
            reader_thread.join().unwrap();
            let lines = stderr_lines.lock().unwrap();
            panic!(
                "ember-vz exited unexpectedly with {status}.\nstderr:\n{}",
                lines.join("\n")
            );
        }

        std::thread::sleep(Duration::from_millis(250));
    }

    // Allow serial output to flush after boot.
    if booted {
        std::thread::sleep(Duration::from_secs(2));
    }

    // --- Stop the VM ---

    eprintln!("Stopping VM (pid {child_pid})...");
    send_sigterm(child_pid);
    let exit_status = wait_with_timeout(&mut child, STOP_TIMEOUT);
    reader_thread.join().unwrap();
    eprintln!("VM exited with: {exit_status}");

    // --- Assertions ---

    let lines = stderr_lines.lock().unwrap();

    // 1. VM must have booted within the timeout.
    assert!(
        booted,
        "VM did not boot within {BOOT_TIMEOUT:?}.\nstderr:\n{}",
        lines.join("\n")
    );

    // 2. Verify MAC address was reported on stderr.
    // ember-vz writes "MAC=<addr>" to stderr when the VM config is built.
    // This proves the vmnet network device was attached to the VM.
    let mac_line = lines.iter().find(|l| l.starts_with("MAC="));
    assert!(
        mac_line.is_some(),
        "expected MAC=<addr> on stderr (vmnet not configured?).\nstderr:\n{}",
        lines.join("\n")
    );
    let mac = mac_line.unwrap().strip_prefix("MAC=").unwrap();
    assert!(
        mac.contains(':') && mac.len() >= 11,
        "invalid MAC address format: '{mac}'"
    );
    eprintln!("Verified MAC address: {mac}");

    // 3. Verify serial console log contains kernel boot messages.
    let serial = std::fs::read_to_string(&serial_log).unwrap_or_default();
    assert!(
        !serial.is_empty(),
        "serial log is empty — virtio console not working"
    );
    assert!(
        serial.contains("Linux version"),
        "serial log should contain 'Linux version' kernel banner.\n\
         First 500 chars of serial log:\n{}",
        &serial[..serial.len().min(500)]
    );
    eprintln!(
        "Verified serial output ({} bytes, contains kernel boot messages)",
        serial.len()
    );

    // 4. Verify graceful shutdown (SIGTERM → exit 0).
    assert!(
        exit_status.success(),
        "ember-vz exited with non-zero status: {exit_status}"
    );
    // Also verify "vm stopped" or SIGTERM acknowledgement on stderr.
    assert!(
        lines
            .iter()
            .any(|l| l.contains("vm stopped") || l.contains("SIGTERM")),
        "expected graceful shutdown message on stderr.\nstderr:\n{}",
        lines.join("\n")
    );

    eprintln!("All checks passed: boot, serial output, network, graceful shutdown");
}
