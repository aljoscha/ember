//! Integration tests for the macOS VM backend (Phase 3).
//!
//! Exercises the full VM lifecycle via the `ember-vz` helper: start with
//! ready-fd pipe (MAC address), is_running check, pause/resume via signals,
//! and graceful/forceful stop.
//!
//! Requirements:
//! - macOS 13+ with Virtualization.framework
//! - `ember-vz` binary built (`swift build` in ember-vz/)
//! - AVF-compatible kernel (see `ensure_kernel()` resolution order)
//! - Homebrew `e2fsprogs` (for mkfs.ext4)
//! - No root required
//!
//! Note: SSH testing requires Phase 4 networking (guest IP discovery
//! from vmnet DHCP leases). Those tests will be in the Phase 4 suite.
//!
//! To run:
//!   ./run-integration-tests.sh macos_vm
#![cfg(target_os = "macos")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

/// Check if a process is alive via kill(pid, 0).
fn is_running(pid: u32) -> bool {
    unsafe { nix::libc::kill(pid as i32, 0) == 0 }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full VM lifecycle: start (with ready-fd) → is_running → stop (SIGTERM).
///
/// This tests the same flow as `MacosVm::start` / `MacosVm::stop`:
/// 1. Spawn ember-vz with --ready-fd pipe
/// 2. Read MAC address from pipe (proves VM booted)
/// 3. Verify process is running via kill(pid, 0)
/// 4. Send SIGTERM for graceful shutdown
/// 5. Verify clean exit
#[test]
#[ignore]
fn vm_lifecycle_start_stop() {
    let ember_vz = match common::ember_vz_bin() {
        Some(p) => {
            eprintln!("Using ember-vz: {}", p.display());
            p
        }
        None => {
            eprintln!("Skipping: ember-vz not found");
            return;
        }
    };

    let kernel = match common::ensure_kernel() {
        Some(k) => {
            eprintln!("Using kernel: {}", k.display());
            k
        }
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let rootfs = common::create_test_rootfs(tmp.path(), 64);
    let serial_log = tmp.path().join("console.log");

    // --- Start with ready-fd ---

    eprintln!("Starting VM with ready-fd pipe...");
    let (mut child, pid, read_file) = common::spawn_ember_vz(
        &ember_vz,
        &kernel,
        &rootfs,
        &serial_log,
        "console=hvc0 root=/dev/vda rw",
    );

    // Read MAC from ready-fd (same as MacosVm::start does).
    let mac = match common::read_mac_from_pipe(read_file, BOOT_TIMEOUT) {
        Some(m) => m,
        None => {
            eprintln!("Failed to read MAC from ready-fd — VM may have crashed");
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            let _ = common::wait_for_exit(&mut child, STOP_TIMEOUT);
            panic!("VM failed to boot (no MAC on ready-fd)");
        }
    };

    eprintln!("VM booted, MAC: {mac}");

    // Verify MAC format.
    assert!(
        mac.contains(':') && mac.len() >= 11,
        "invalid MAC address: '{mac}'"
    );

    // Verify process is running (same as MacosVm::is_running).
    assert!(is_running(pid), "ember-vz should be running after boot");

    // Let the VM run for a bit.
    std::thread::sleep(Duration::from_secs(2));
    assert!(is_running(pid), "ember-vz should still be running");

    // --- Stop (SIGTERM + wait) ---

    eprintln!("Stopping VM (SIGTERM)...");
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM).expect("SIGTERM failed");
    let status = common::wait_for_exit(&mut child, STOP_TIMEOUT);
    eprintln!("VM exited: {status}");

    assert!(status.success(), "expected clean exit, got {status}");
    assert!(!is_running(pid), "process should be gone after stop");

    // --- Verify serial output ---

    if serial_log.exists() {
        let serial = std::fs::read_to_string(&serial_log).unwrap_or_default();
        if !serial.is_empty() {
            assert!(
                serial.contains("virtio_blk") || serial.contains("Linux version"),
                "serial log should contain kernel boot messages.\nFirst 300 chars:\n{}",
                &serial[..serial.len().min(300)]
            );
            eprintln!("Serial output verified ({} bytes)", serial.len());
        }
    }

    eprintln!("VM lifecycle test passed: start → ready-fd → is_running → stop");
}

/// Force stop: SIGKILL kills the VM immediately.
#[test]
#[ignore]
fn vm_force_stop() {
    let ember_vz = match common::ember_vz_bin() {
        Some(p) => p,
        None => {
            eprintln!("Skipping: ember-vz not found");
            return;
        }
    };
    let kernel = match common::ensure_kernel() {
        Some(k) => k,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let rootfs = common::create_test_rootfs(tmp.path(), 64);
    let serial_log = tmp.path().join("console.log");

    let (mut child, pid, read_file) = common::spawn_ember_vz(
        &ember_vz,
        &kernel,
        &rootfs,
        &serial_log,
        "console=hvc0 root=/dev/vda rw",
    );

    // Wait for boot.
    let _mac = common::read_mac_from_pipe(read_file, BOOT_TIMEOUT).expect("VM failed to boot");
    assert!(is_running(pid));

    // Force stop (SIGKILL — same as MacosVm::force_stop).
    eprintln!("Sending SIGKILL...");
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL).expect("SIGKILL failed");
    let _ = common::wait_for_exit(&mut child, Duration::from_secs(5));

    assert!(!is_running(pid), "process should be dead after SIGKILL");
    eprintln!("Force stop test passed");
}

/// Pause (SIGUSR1) and resume (SIGUSR2) keep the process alive.
#[test]
#[ignore]
fn vm_pause_resume() {
    let ember_vz = match common::ember_vz_bin() {
        Some(p) => p,
        None => {
            eprintln!("Skipping: ember-vz not found");
            return;
        }
    };
    let kernel = match common::ensure_kernel() {
        Some(k) => k,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let rootfs = common::create_test_rootfs(tmp.path(), 64);
    let serial_log = tmp.path().join("console.log");

    let (mut child, pid, read_file) = common::spawn_ember_vz(
        &ember_vz,
        &kernel,
        &rootfs,
        &serial_log,
        "console=hvc0 root=/dev/vda rw",
    );

    let _mac = common::read_mac_from_pipe(read_file, BOOT_TIMEOUT).expect("VM failed to boot");

    std::thread::sleep(Duration::from_secs(2));

    // Pause.
    eprintln!("Sending SIGUSR1 (pause)...");
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGUSR1).expect("SIGUSR1 failed");
    std::thread::sleep(Duration::from_secs(1));
    assert!(is_running(pid), "process should be alive while paused");

    // Resume.
    eprintln!("Sending SIGUSR2 (resume)...");
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGUSR2).expect("SIGUSR2 failed");
    std::thread::sleep(Duration::from_secs(1));
    assert!(is_running(pid), "process should be alive after resume");

    // Clean shutdown.
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM).expect("SIGTERM failed");
    let _ = common::wait_for_exit(&mut child, STOP_TIMEOUT);
    assert!(!is_running(pid));

    eprintln!("Pause/resume test passed");
}
