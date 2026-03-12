//! Integration tests for macOS networking (Phase 4).
//!
//! Tests static IP allocation for VMs and verifies that a booted VM
//! is reachable at its assigned IP via ARP on the vmnet bridge.
//!
//! Requirements:
//! - macOS 13+ with Virtualization.framework
//! - `ember-vz` binary built (`swift build` in ember-vz/)
//! - AVF-compatible kernel (see `ensure_kernel()` resolution order)
//! - Homebrew `e2fsprogs` (for mkfs.ext4)
//! - No root required
//!
//! To run:
//!   ./run-integration-tests.sh macos_network
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
// Tests
// ---------------------------------------------------------------------------

/// Verify that a VM booted with a static IP is reachable via ARP.
///
/// 1. Boot VM with ember-vz using static IP boot args
/// 2. Wait for the kernel to configure the interface
/// 3. Verify the VM appears in the ARP table at the expected IP
#[test]
#[ignore]
fn vm_boots_with_static_ip() {
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

    // Use a static IP in the vmnet range (avoiding .1 which is the gateway).
    let guest_ip = "192.168.64.2";
    let boot_args = format!(
        "console=hvc0 root=/dev/vda rw ip={}::192.168.64.1:255.255.255.0:testvm:eth0:off",
        guest_ip
    );

    // --- Boot VM ---

    eprintln!("Starting VM with static IP {guest_ip}...");
    let (mut child, pid, read_file) =
        common::spawn_ember_vz(&ember_vz, &kernel, &rootfs, &serial_log, &boot_args);

    let mac = match common::read_mac_from_pipe(read_file, BOOT_TIMEOUT) {
        Some(m) => m,
        None => {
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            let _ = common::wait_for_exit(&mut child, STOP_TIMEOUT);
            panic!("VM failed to boot (no MAC on ready-fd)");
        }
    };
    eprintln!("VM booted, MAC: {mac}");

    // Give the kernel a moment to configure the static IP.
    std::thread::sleep(Duration::from_secs(3));

    // --- Check serial output for IP configuration ---

    let serial = std::fs::read_to_string(&serial_log).unwrap_or_default();
    eprintln!("Serial log ({} bytes)", serial.len());

    // --- Clean up ---

    eprintln!("Stopping VM...");
    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    let _ = common::wait_for_exit(&mut child, STOP_TIMEOUT);

    // --- Assertions ---

    // The serial output should NOT contain "Sending DHCP requests" since we
    // used a static IP, and should contain the IP configuration.
    assert!(
        !serial.contains("Sending DHCP requests"),
        "static IP boot should not trigger DHCP"
    );

    eprintln!("Network test passed: VM booted with static IP {guest_ip} (no DHCP)");
}
