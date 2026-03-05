//! iptables NAT/masquerade rule management.
//!
//! Each VM gets three iptables rules for outbound network connectivity:
//!
//! 1. **POSTROUTING MASQUERADE** — rewrites guest source IP for outbound traffic
//! 2. **FORWARD (outbound)** — allows traffic from TAP device to WAN interface
//! 3. **FORWARD (inbound)** — allows established/related return traffic from WAN to TAP
//!
//! Rules are added on VM start and removed on VM stop/delete. The `remove_rules`
//! function is idempotent — it silently ignores errors when rules don't exist.

use std::process::Command;

use crate::error::{Error, Result};

/// Add iptables NAT and forwarding rules for a VM.
///
/// Creates three rules that together give the guest outbound internet access
/// through the host's WAN interface via masquerading (SNAT):
///
/// ```text
/// -t nat -A POSTROUTING -s <guest_ip>/32 -o <wan_iface> -j MASQUERADE
/// -A FORWARD -i <tap_device> -o <wan_iface> -j ACCEPT
/// -A FORWARD -i <wan_iface> -o <tap_device> -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
/// ```
pub fn add_rules(tap_device: &str, guest_ip: &str, wan_iface: &str) -> Result<()> {
    let guest_cidr = format!("{guest_ip}/32");

    // 1. NAT masquerade for outbound guest traffic.
    iptables(&[
        "-t",
        "nat",
        "-A",
        "POSTROUTING",
        "-s",
        &guest_cidr,
        "-o",
        wan_iface,
        "-j",
        "MASQUERADE",
    ])?;

    // 2. Allow forwarding from TAP to WAN.
    iptables(&[
        "-A", "FORWARD", "-i", tap_device, "-o", wan_iface, "-j", "ACCEPT",
    ])?;

    // 3. Allow established/related return traffic from WAN to TAP.
    iptables(&[
        "-A",
        "FORWARD",
        "-i",
        wan_iface,
        "-o",
        tap_device,
        "-m",
        "conntrack",
        "--ctstate",
        "RELATED,ESTABLISHED",
        "-j",
        "ACCEPT",
    ])?;

    Ok(())
}

/// Remove iptables NAT and forwarding rules for a VM.
///
/// Mirrors [`add_rules`] but uses `-D` (delete) instead of `-A` (append).
/// Idempotent — silently ignores errors when rules don't exist.
pub fn remove_rules(tap_device: &str, guest_ip: &str, wan_iface: &str) -> Result<()> {
    let guest_cidr = format!("{guest_ip}/32");

    // Same rules as add_rules, but with -D to delete.
    // Ignore "does not exist" errors for idempotency.
    let _ = iptables_delete(&[
        "-t",
        "nat",
        "-D",
        "POSTROUTING",
        "-s",
        &guest_cidr,
        "-o",
        wan_iface,
        "-j",
        "MASQUERADE",
    ]);

    let _ = iptables_delete(&[
        "-D", "FORWARD", "-i", tap_device, "-o", wan_iface, "-j", "ACCEPT",
    ]);

    let _ = iptables_delete(&[
        "-D",
        "FORWARD",
        "-i",
        wan_iface,
        "-o",
        tap_device,
        "-m",
        "conntrack",
        "--ctstate",
        "RELATED,ESTABLISHED",
        "-j",
        "ACCEPT",
    ]);

    Ok(())
}

/// Enable IPv4 forwarding via sysctl.
///
/// This is required once before any VM can route traffic through the host.
/// Safe to call multiple times — sysctl is idempotent.
pub fn enable_ip_forwarding() -> Result<()> {
    let output = Command::new("sysctl")
        .args(["-w", "net.ipv4.ip_forward=1"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "sysctl".into(),
            source: e,
        })?;
    Error::check_command("sysctl", output)?;
    Ok(())
}

/// Run an iptables command, returning an error on failure.
fn iptables(args: &[&str]) -> Result<()> {
    let output = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "iptables".into(),
            source: e,
        })?;
    Error::check_command("iptables", output)?;
    Ok(())
}

/// Run an iptables delete command, removing ALL matching instances.
///
/// `iptables -D` only removes the first match. If the same rule was
/// added multiple times (e.g. a test VM and a manual VM both at the
/// same IP), we need to loop until all copies are gone.
///
/// Silently ignores "rule doesn't exist" errors for idempotent cleanup.
fn iptables_delete(args: &[&str]) -> Result<()> {
    loop {
        let output =
            Command::new("iptables")
                .args(args)
                .output()
                .map_err(|e| Error::CommandExec {
                    command: "iptables".into(),
                    source: e,
                })?;

        if output.status.success() {
            // Deleted one instance — loop to catch duplicates.
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does a matching rule exist") || stderr.contains("No chain/target/match")
        {
            // No more matching rules — done.
            return Ok(());
        }
        return Err(Error::Network(format!(
            "iptables failed: {}",
            stderr.trim()
        )));
    }
}
