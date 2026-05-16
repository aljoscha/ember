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

use ember_core::error::{Error, Result};

/// iptables comment that scopes rule cleanup to one ember install.
///
/// `Some(ns)` → `ember:{ns}`, embedded via `-m comment --comment` in
/// every rule so `-D` only matches *this* install's rules. `None`
/// returns the empty string, which [`with_comment`] uses as the
/// signal to omit the `-m comment` match entirely — older binaries
/// added rules without a comment match, so emitting one on legacy
/// installs would make `iptables -D` silently no-op and rules would
/// accumulate forever. Empty preserves the original rule shape.
pub fn comment(instance_id: Option<&str>) -> String {
    match instance_id {
        None => String::new(),
        Some(id) => format!("ember:{id}"),
    }
}

/// Add iptables NAT and forwarding rules for a VM.
///
/// Creates three rules that together give the guest outbound internet access
/// through the host's WAN interface via masquerading (SNAT):
///
/// ```text
/// -t nat -A POSTROUTING -s <guest_ip>/32 -o <wan_iface> [-m comment --comment <c>] -j MASQUERADE
/// -A FORWARD -i <tap_device> -o <wan_iface> [-m comment --comment <c>] -j ACCEPT
/// -A FORWARD -i <wan_iface> -o <tap_device> -m conntrack --ctstate RELATED,ESTABLISHED [-m comment --comment <c>] -j ACCEPT
/// ```
///
/// `comment` is a per-installation tag (e.g. `ember:a3f4`) embedded in
/// every rule via the `comment` match. It lets cleanup scope deletions
/// to this installation's rules and lets users grep `iptables-save` for
/// "rules ember put here". An empty `comment` skips the match entirely
/// so rules added by older ember binaries (which never tagged anything)
/// stay byte-for-byte identical and remain matchable by `remove_rules`.
pub fn add_rules(tap_device: &str, guest_ip: &str, wan_iface: &str, comment: &str) -> Result<()> {
    let guest_cidr = format!("{guest_ip}/32");

    iptables(&with_comment(
        &[
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-s",
            &guest_cidr,
            "-o",
            wan_iface,
        ],
        comment,
        &["-j", "MASQUERADE"],
    ))?;

    iptables(&with_comment(
        &["-A", "FORWARD", "-i", tap_device, "-o", wan_iface],
        comment,
        &["-j", "ACCEPT"],
    ))?;

    iptables(&with_comment(
        &[
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
        ],
        comment,
        &["-j", "ACCEPT"],
    ))?;

    Ok(())
}

/// Remove iptables NAT and forwarding rules for a VM.
///
/// Mirrors [`add_rules`] but uses `-D` (delete) instead of `-A` (append).
/// Idempotent — silently ignores errors when rules don't exist. The
/// `comment` argument must match the value passed to [`add_rules`];
/// iptables compares the full rule including the comment match, so a
/// wrong tag turns the delete into a no-op rather than removing
/// another install's rule.
pub fn remove_rules(
    tap_device: &str,
    guest_ip: &str,
    wan_iface: &str,
    comment: &str,
) -> Result<()> {
    let guest_cidr = format!("{guest_ip}/32");

    let _ = iptables_delete(&with_comment(
        &[
            "-t",
            "nat",
            "-D",
            "POSTROUTING",
            "-s",
            &guest_cidr,
            "-o",
            wan_iface,
        ],
        comment,
        &["-j", "MASQUERADE"],
    ));

    let _ = iptables_delete(&with_comment(
        &["-D", "FORWARD", "-i", tap_device, "-o", wan_iface],
        comment,
        &["-j", "ACCEPT"],
    ));

    let _ = iptables_delete(&with_comment(
        &[
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
        ],
        comment,
        &["-j", "ACCEPT"],
    ));

    Ok(())
}

/// Splice `-m comment --comment <comment>` between rule head and tail
/// when `comment` is non-empty. Empty comment yields the unwrapped
/// rule, matching what older ember binaries emitted byte-for-byte —
/// crucial because iptables compares full rules during `-D`.
fn with_comment<'a>(head: &[&'a str], comment: &'a str, tail: &[&'a str]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(head.len() + tail.len() + 4);
    out.extend_from_slice(head);
    if !comment.is_empty() {
        out.extend_from_slice(&["-m", "comment", "--comment", comment]);
    }
    out.extend_from_slice(tail);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_for_new_install_tags_namespace() {
        assert_eq!(comment(Some("a3f4")), "ember:a3f4");
    }

    /// Locked: legacy mode must return an empty string so the rule
    /// shape stays byte-for-byte identical to what older binaries
    /// emitted (no `-m comment` match), or `iptables -D` silently
    /// no-ops on upgraded hosts.
    #[test]
    fn comment_for_legacy_install_is_empty() {
        assert_eq!(comment(None), "");
    }

    #[test]
    fn with_comment_skips_match_when_empty() {
        // Legacy mode (empty comment) must produce byte-for-byte the
        // same rule the old binary added, otherwise `iptables -D`
        // won't match existing rules on upgraded hosts.
        let args = with_comment(&["-A", "FORWARD", "-i", "tap0"], "", &["-j", "ACCEPT"]);
        assert_eq!(args, vec!["-A", "FORWARD", "-i", "tap0", "-j", "ACCEPT"]);
    }

    #[test]
    fn with_comment_inserts_comment_match_when_non_empty() {
        let args = with_comment(
            &["-A", "FORWARD", "-i", "tap0"],
            "ember:a3f4",
            &["-j", "ACCEPT"],
        );
        assert_eq!(
            args,
            vec![
                "-A",
                "FORWARD",
                "-i",
                "tap0",
                "-m",
                "comment",
                "--comment",
                "ember:a3f4",
                "-j",
                "ACCEPT"
            ]
        );
    }
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
