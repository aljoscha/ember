//! WAN interface detection.
//!
//! Detects the host's default outbound network interface by querying the
//! routing table. The result is cached in [`GlobalConfig`] at `ember init`
//! time and can be overridden via the `--wan-iface` CLI flag.

use std::process::Command;

use crate::error::{Error, Result};

/// Detect the default WAN interface by querying the routing table.
///
/// Runs `ip route get 8.8.8.8` and parses the `dev <iface>` field from the
/// output. This identifies which interface the kernel would use to reach an
/// external address — i.e., the default outbound interface.
///
/// # Errors
///
/// Returns [`Error::Network`] if:
/// - The `ip` command cannot be executed
/// - The command exits with a non-zero status
/// - The output does not contain a `dev` field (e.g., no default route)
pub fn detect() -> Result<String> {
    let output = Command::new("ip")
        .args(["route", "get", "8.8.8.8"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "ip".into(),
            source: e,
        })?;

    let output = Error::check_command("ip route get", output)?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    parse_dev_from_route(&stdout).ok_or_else(|| {
        Error::Network(format!(
            "could not determine WAN interface from `ip route get 8.8.8.8` output: {stdout}"
        ))
    })
}

/// Parse the `dev <iface>` token from `ip route get` output.
///
/// Example output:
/// ```text
/// 8.8.8.8 via 192.168.1.1 dev wlp2s0 src 192.168.1.100 uid 1000
/// ```
fn parse_dev_from_route(output: &str) -> Option<String> {
    let mut tokens = output.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "dev" {
            return tokens.next().map(|s| s.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_route_output() {
        let output = "8.8.8.8 via 192.168.1.1 dev wlp2s0 src 192.168.1.100 uid 1000\n";
        assert_eq!(parse_dev_from_route(output), Some("wlp2s0".to_string()));
    }

    #[test]
    fn parse_ethernet_interface() {
        let output = "8.8.8.8 via 10.0.0.1 dev eth0 src 10.0.0.50 uid 0\n";
        assert_eq!(parse_dev_from_route(output), Some("eth0".to_string()));
    }

    #[test]
    fn parse_with_cache_line() {
        // Some kernels include a "cache" line.
        let output = "8.8.8.8 via 192.168.0.1 dev enp3s0 src 192.168.0.10 uid 1000\n    cache\n";
        assert_eq!(parse_dev_from_route(output), Some("enp3s0".to_string()));
    }

    #[test]
    fn parse_no_dev_field() {
        let output = "unreachable\n";
        assert_eq!(parse_dev_from_route(output), None);
    }

    #[test]
    fn parse_empty_output() {
        assert_eq!(parse_dev_from_route(""), None);
    }

    #[test]
    fn parse_dev_at_end() {
        let output = "8.8.8.8 dev lo src 127.0.0.1\n";
        assert_eq!(parse_dev_from_route(output), Some("lo".to_string()));
    }
}
