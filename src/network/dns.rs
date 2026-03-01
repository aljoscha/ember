//! Host DNS server detection for guest VM configuration.
//!
//! Reads the host's DNS nameservers so they can be passed to guest VMs
//! via the kernel `ip=` boot parameter. The kernel writes these to
//! `/proc/net/pnp`, which the guest symlinks as `/etc/resolv.conf`.

use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::Command;

/// Fallback DNS servers when host detection fails.
const FALLBACK_NAMESERVERS: &[&str] = &["1.1.1.1", "8.8.8.8"];

/// Maximum number of DNS servers to return (kernel ip= supports 2).
const MAX_NAMESERVERS: usize = 2;

/// Detect the host's DNS nameservers for a specific WAN interface.
///
/// VM traffic is NATed through the WAN interface, so only DNS servers
/// reachable via that interface are useful. Using servers from other
/// interfaces (e.g. LAN router DNS when traffic goes through a VPN)
/// causes ~10s timeouts per query.
///
/// Resolution order:
/// 1. `resolvectl dns <wan_iface>` — DNS servers for the specific
///    interface (most accurate, avoids unreachable servers)
/// 2. `/run/systemd/resolve/resolv.conf` — all upstream servers from
///    systemd-resolved (avoids the 127.0.0.53 stub)
/// 3. `/etc/resolv.conf` — direct resolv.conf
/// 4. Fallback to 1.1.1.1 + 8.8.8.8
///
/// Filters out IPv6 addresses (VMs only have IPv4) and loopback
/// addresses (unreachable from the guest).
pub fn detect_nameservers(wan_iface: &str) -> Vec<String> {
    // Try interface-specific DNS via resolvectl (most accurate).
    if let Some(servers) = resolvectl_dns(wan_iface) {
        if !servers.is_empty() {
            return servers;
        }
    }

    // Fall back to systemd-resolved upstream config.
    if let Some(servers) = parse_resolv_conf(Path::new("/run/systemd/resolve/resolv.conf")) {
        if !servers.is_empty() {
            return servers;
        }
    }

    // Fall back to /etc/resolv.conf.
    if let Some(servers) = parse_resolv_conf(Path::new("/etc/resolv.conf")) {
        if !servers.is_empty() {
            return servers;
        }
    }

    // Last resort: hardcoded public DNS.
    FALLBACK_NAMESERVERS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// Query DNS servers for a specific interface via `resolvectl dns`.
///
/// Output format: `Link N (ifname): 10.64.0.1 fc00:bbbb::1`
fn resolvectl_dns(iface: &str) -> Option<Vec<String>> {
    let output = Command::new("resolvectl")
        .args(["dns", iface])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The servers are space-separated after the colon.
    let servers_part = stdout.split_once(':')?.1;
    let servers: Vec<String> = servers_part
        .split_whitespace()
        .filter(|addr| is_usable_ipv4(addr))
        .take(MAX_NAMESERVERS)
        .map(|s| s.to_string())
        .collect();
    Some(servers)
}

/// Parse nameserver entries from a resolv.conf file.
///
/// Returns `None` if the file doesn't exist or can't be read.
/// Returns `Some(vec)` with usable IPv4 nameservers (may be empty).
fn parse_resolv_conf(path: &Path) -> Option<Vec<String>> {
    let contents = fs::read_to_string(path).ok()?;
    let servers: Vec<String> = contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let addr = line.strip_prefix("nameserver")?.trim();
            // Only keep usable IPv4 addresses.
            if is_usable_ipv4(addr) {
                Some(addr.to_string())
            } else {
                None
            }
        })
        .take(MAX_NAMESERVERS)
        .collect();
    Some(servers)
}

/// Check if an address is a usable IPv4 nameserver for a guest VM.
///
/// Rejects:
/// - IPv6 addresses (contain `:`)
/// - Loopback addresses (`127.x.x.x`) — unreachable from the guest
/// - Unparseable strings
fn is_usable_ipv4(addr: &str) -> bool {
    // Reject IPv6 (contains colons) and anything with extra suffixes
    // like "9.9.9.9#dns.quad9.net".
    let addr = addr.split('#').next().unwrap_or(addr);
    match addr.parse::<Ipv4Addr>() {
        Ok(ip) => !ip.is_loopback(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_usable_filters_loopback() {
        assert!(!is_usable_ipv4("127.0.0.1"));
        assert!(!is_usable_ipv4("127.0.0.53"));
    }

    #[test]
    fn is_usable_filters_ipv6() {
        assert!(!is_usable_ipv4("::1"));
        assert!(!is_usable_ipv4("2606:4700:4700::1111"));
        assert!(!is_usable_ipv4("fc00:bbbb:bbbb:bb01::1"));
    }

    #[test]
    fn is_usable_accepts_public_ipv4() {
        assert!(is_usable_ipv4("1.1.1.1"));
        assert!(is_usable_ipv4("8.8.8.8"));
        assert!(is_usable_ipv4("10.64.0.1"));
        assert!(is_usable_ipv4("192.168.0.1"));
    }

    #[test]
    fn is_usable_handles_hash_suffix() {
        assert!(is_usable_ipv4("9.9.9.9#dns.quad9.net"));
        assert!(is_usable_ipv4("1.1.1.1#cloudflare-dns.com"));
    }

    #[test]
    fn is_usable_rejects_garbage() {
        assert!(!is_usable_ipv4("not-an-ip"));
        assert!(!is_usable_ipv4(""));
    }

    #[test]
    fn parse_resolv_conf_extracts_nameservers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolv.conf");
        std::fs::write(
            &path,
            "# comment\nnameserver 10.64.0.1\nnameserver 192.168.0.1\nsearch home\n",
        )
        .unwrap();

        let servers = parse_resolv_conf(&path).unwrap();
        assert_eq!(servers, vec!["10.64.0.1", "192.168.0.1"]);
    }

    #[test]
    fn parse_resolv_conf_filters_ipv6_and_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolv.conf");
        std::fs::write(
            &path,
            "nameserver 127.0.0.53\nnameserver 2606:4700::1\nnameserver 8.8.8.8\n",
        )
        .unwrap();

        let servers = parse_resolv_conf(&path).unwrap();
        assert_eq!(servers, vec!["8.8.8.8"]);
    }

    #[test]
    fn parse_resolv_conf_limits_to_max() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resolv.conf");
        std::fs::write(
            &path,
            "nameserver 1.1.1.1\nnameserver 8.8.8.8\nnameserver 9.9.9.9\n",
        )
        .unwrap();

        let servers = parse_resolv_conf(&path).unwrap();
        assert_eq!(servers.len(), 2);
    }

    #[test]
    fn parse_resolv_conf_missing_file() {
        assert!(parse_resolv_conf(Path::new("/nonexistent/resolv.conf")).is_none());
    }

    #[test]
    fn detect_nameservers_returns_something() {
        // Use a bogus interface — should fall back to resolv.conf or hardcoded.
        let servers = detect_nameservers("nonexistent0");
        assert!(!servers.is_empty());
        assert!(servers.len() <= MAX_NAMESERVERS);
    }

    #[test]
    fn resolvectl_dns_nonexistent_iface() {
        // Should return None or empty, not panic.
        let result = resolvectl_dns("nonexistent0");
        assert!(result.is_none() || result.unwrap().is_empty());
    }

    #[test]
    fn resolvectl_parses_output() {
        // Simulates: "Link 4 (wg0-mullvad): 10.64.0.1 fc00:bbbb:bbbb:bb01::1"
        let output = "Link 4 (wg0-mullvad): 10.64.0.1 fc00:bbbb:bbbb:bb01::1";
        let servers_part = output.split_once(':').unwrap().1;
        let servers: Vec<String> = servers_part
            .split_whitespace()
            .filter(|addr| is_usable_ipv4(addr))
            .take(MAX_NAMESERVERS)
            .map(|s| s.to_string())
            .collect();
        assert_eq!(servers, vec!["10.64.0.1"]);
    }
}
