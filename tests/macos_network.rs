//! Integration tests for macOS networking (Phase 4).
//!
//! Tests guest IP discovery from vmnet DHCP leases after booting a VM
//! with `ember-vz`. Verifies that the network backend can find the
//! guest's IP address using the MAC reported via ready-fd.
//!
//! Requirements:
//! - macOS 13+ with Virtualization.framework
//! - `ember-vz` binary built (`swift build` in ember-vz/)
//! - AVF-compatible kernel (see `ensure_kernel()` resolution order)
//! - Homebrew `e2fsprogs` (for mkfs.ext4)
//! - No root required
//!
//! Note: SSH and internet-from-guest tests require a full rootfs with
//! sshd and networking tools (Phase 5 image pipeline). This suite tests
//! the DHCP IP discovery path only.
//!
//! To run:
//!   ./run-integration-tests.sh macos_network
#![cfg(target_os = "macos")]

#[allow(dead_code)]
mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the guest to obtain a DHCP lease after boot.
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// IP Discovery Helpers (mirrors logic in backend::macos::network)
// ---------------------------------------------------------------------------

/// Search /var/db/dhcpd_leases for the given MAC address.
fn find_ip_in_dhcp_leases(mac: &str) -> Option<String> {
    let contents = std::fs::read_to_string("/var/db/dhcpd_leases").ok()?;
    let mac_lower = mac.to_lowercase();

    let mut ip: Option<String> = None;
    let mut hw_mac: Option<String> = None;

    for line in contents.lines() {
        let line = line.trim();
        if line == "{" {
            ip = None;
            hw_mac = None;
        } else if line == "}" {
            if let (Some(ref lease_ip), Some(ref lease_mac)) = (&ip, &hw_mac) {
                if lease_mac == &mac_lower {
                    return Some(lease_ip.clone());
                }
            }
        } else if let Some(value) = line.strip_prefix("ip_address=") {
            ip = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("hw_address=") {
            let mac_part = value.split_once(',').map(|(_, m)| m).unwrap_or(value);
            hw_mac = Some(mac_part.to_lowercase());
        }
    }
    None
}

/// Search `arp -a` output for the given MAC address.
fn find_ip_in_arp(mac: &str) -> Option<String> {
    let output = Command::new("arp").arg("-a").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let normalized_target = normalize_mac(mac);

    for line in stdout.lines() {
        let after_at = line.split(" at ").nth(1)?;
        let arp_mac = after_at.split(" on ").next()?;
        if arp_mac == "(incomplete)" {
            continue;
        }
        if normalize_mac(arp_mac) == normalized_target {
            let start = line.find('(')? + 1;
            let end = line.find(')')?;
            return Some(line[start..end].to_string());
        }
    }
    None
}

/// Normalize MAC: lowercase + zero-pad octets (e.g. "e:49" → "0e:49").
fn normalize_mac(mac: &str) -> String {
    mac.to_lowercase()
        .split(':')
        .map(|o| format!("{:0>2}", o))
        .collect::<Vec<_>>()
        .join(":")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verify that a booted VM gets a DHCP IP from vmnet and we can discover it.
///
/// 1. Boot VM with ember-vz, get MAC from ready-fd
/// 2. Wait for vmnet DHCP to assign an IP
/// 3. Use the network backend's IP discovery (DHCP leases + ARP fallback)
/// 4. Verify the discovered IP is in the vmnet range (192.168.64.x)
#[test]
#[ignore]
fn vm_gets_dhcp_ip() {
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

    // --- Boot VM ---

    eprintln!("Starting VM...");
    let (mut child, pid, read_file) = common::spawn_ember_vz(
        &ember_vz,
        &kernel,
        &rootfs,
        &serial_log,
        "console=hvc0 root=/dev/vda rw ip=dhcp",
    );

    let mac = match common::read_mac_from_pipe(read_file, BOOT_TIMEOUT) {
        Some(m) => m,
        None => {
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            let _ = common::wait_for_exit(&mut child, STOP_TIMEOUT);
            panic!("VM failed to boot (no MAC on ready-fd)");
        }
    };
    eprintln!("VM booted, MAC: {mac}");

    // --- Discover guest IP ---

    // Poll DHCP leases and ARP table for the guest's MAC address.
    // The guest needs a moment to complete DHCP negotiation.
    let mut discovered_ip: Option<String> = None;
    let start = Instant::now();
    while start.elapsed() < DHCP_TIMEOUT {
        // Strategy 1: DHCP leases file.
        if let Some(ip) = find_ip_in_dhcp_leases(&mac) {
            discovered_ip = Some(ip);
            break;
        }
        // Strategy 2: ARP table.
        if let Some(ip) = find_ip_in_arp(&mac) {
            discovered_ip = Some(ip);
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    // --- Clean up ---

    eprintln!("Stopping VM...");
    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    let _ = common::wait_for_exit(&mut child, STOP_TIMEOUT);

    // --- Assertions ---

    let ip = discovered_ip.expect(
        "failed to discover guest IP within timeout — \
         vmnet DHCP may not have assigned a lease",
    );

    eprintln!("Discovered guest IP: {ip}");

    // Verify the IP is in the vmnet shared mode range (192.168.64.0/24).
    assert!(
        ip.starts_with("192.168.64."),
        "expected IP in 192.168.64.0/24 range, got {ip}"
    );

    // Verify it's not the gateway.
    assert_ne!(ip, "192.168.64.1", "guest IP should not be the gateway");

    eprintln!("Network test passed: VM got DHCP IP {ip} via vmnet");
}
