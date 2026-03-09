//! macOS network backend: vmnet shared mode.
//!
//! vmnet provides NAT + DHCP automatically — most operations are no-ops.
//! The main work is discovering the guest IP from DHCP leases after boot.

use std::path::PathBuf;
use std::process::Command;

use crate::backend::NetworkBackend;
use crate::cli::init::GlobalConfig;
use crate::error::{Error, Result};
use crate::state::vm::{NetworkInfo, VmMetadata};

/// vmnet shared mode defaults.
const VMNET_GATEWAY: &str = "192.168.64.1";

/// Default netmask for vmnet shared mode (/24).
const VMNET_NETMASK: &str = "255.255.255.0";

/// Path to the vmnet DHCP lease file maintained by macOS.
const DHCP_LEASES_PATH: &str = "/var/db/dhcpd_leases";

/// macOS network backend using vmnet (shared mode).
///
/// vmnet handles NAT and DHCP internally, so setup/teardown are no-ops.
/// Guest IP is assigned by vmnet's DHCP server and discovered post-boot
/// via `discover_guest_ip`.
pub struct MacosNetwork;

impl MacosNetwork {
    pub fn new(_store: crate::state::store::StateStore) -> Self {
        Self
    }
}

impl NetworkBackend for MacosNetwork {
    /// No-op on macOS — vmnet creates the virtual network automatically
    /// when AVF starts a VM with VZNATNetworkDeviceAttachment.
    ///
    /// Returns NetworkInfo with the vmnet gateway defaults. The guest IP
    /// is not yet known (DHCP assigns it after boot); it will be filled
    /// in by `discover_guest_ip` once the VM is running.
    fn setup(&self, _vm: &VmMetadata, _config: &GlobalConfig) -> Result<NetworkInfo> {
        Ok(NetworkInfo {
            // No TAP device on macOS — vmnet manages the virtual interface.
            tap_device: String::new(),
            host_ip: VMNET_GATEWAY.to_string(),
            // Guest IP is assigned by vmnet DHCP after boot; placeholder until discovery.
            guest_ip: "pending".to_string(),
            netmask: VMNET_NETMASK.to_string(),
            // MAC address is assigned by AVF/vmnet at VM start time.
            guest_mac: None,
            // No WAN interface tracking needed — vmnet handles NAT internally.
            wan_iface: None,
        })
    }

    fn teardown(&self, _vm: &VmMetadata) -> Result<()> {
        // vmnet cleans up automatically — nothing to do.
        Ok(())
    }

    /// Discover the guest IP by MAC address using two strategies:
    ///
    /// 1. **Primary**: Parse vmnet DHCP lease file (`/var/db/dhcpd_leases`)
    /// 2. **Fallback**: Parse the system ARP table (`arp -a`)
    ///
    /// The DHCP lease file is checked first because it's faster (no subprocess)
    /// and more reliable. The ARP fallback covers cases where the lease file
    /// hasn't been updated yet or was cleared.
    fn discover_guest_ip(&self, mac: &str) -> Result<String> {
        // Strategy 1: DHCP leases file.
        if let Some(ip) = discover_ip_from_dhcp_leases(mac)? {
            return Ok(ip);
        }

        // Strategy 2: ARP table.
        if let Some(ip) = discover_ip_from_arp(mac)? {
            return Ok(ip);
        }

        Err(Error::Network(format!(
            "no IP found for MAC {mac} in DHCP leases or ARP table\n\
             Hint: the VM may not have obtained an IP yet"
        )))
    }
}

/// Detect the default WAN interface on macOS via `route get 8.8.8.8`.
///
/// Parses the `interface: <name>` line from the output. While vmnet handles
/// NAT internally (so the WAN interface isn't needed for firewall rules),
/// this is useful for diagnostics and stored in GlobalConfig for consistency
/// with Linux.
///
/// # Example output
/// ```text
///    route to: dns.google
/// destination: default
///     gateway: 192.168.0.1
///   interface: en0
///       flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>
/// ```
pub fn detect_wan_iface() -> Result<String> {
    let output = Command::new("route")
        .args(["get", "8.8.8.8"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "route".to_string(),
            source: e,
        })?;

    let output = Error::check_command("route get 8.8.8.8", output)?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    parse_interface_from_route(&stdout).ok_or_else(|| {
        Error::Network(
            "could not detect default network interface — is the host connected to the internet?\n\
             Hint: specify the interface manually with: ember init --wan-iface <iface>"
                .to_string(),
        )
    })
}

/// Parse the `interface: <name>` field from macOS `route get` output.
fn parse_interface_from_route(output: &str) -> Option<String> {
    for line in output.lines() {
        let line = line.trim();
        if let Some(iface) = line.strip_prefix("interface:") {
            let iface = iface.trim();
            if !iface.is_empty() {
                return Some(iface.to_string());
            }
        }
    }
    None
}

/// Try to find the guest IP from the vmnet DHCP lease file.
///
/// Returns `Ok(None)` if the lease file exists but doesn't contain the MAC.
/// Returns `Err` only if the file can't be read (not ENOENT — missing file
/// returns `Ok(None)` since vmnet may not have written it yet).
fn discover_ip_from_dhcp_leases(mac: &str) -> Result<Option<String>> {
    let lease_path = PathBuf::from(DHCP_LEASES_PATH);
    match std::fs::read_to_string(&lease_path) {
        Ok(contents) => Ok(find_ip_in_dhcp_leases(&contents, mac)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io {
            path: lease_path,
            source: e,
        }),
    }
}

/// Try to find the guest IP from the system ARP table (`arp -a`).
///
/// Returns `Ok(None)` if the MAC isn't found in the ARP table.
fn discover_ip_from_arp(mac: &str) -> Result<Option<String>> {
    let output = Command::new("arp")
        .arg("-a")
        .output()
        .map_err(|e| Error::CommandExec {
            command: "arp".to_string(),
            source: e,
        })?;

    if !output.status.success() {
        return Err(Error::Command {
            command: "arp -a".to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(find_ip_in_arp_output(&stdout, mac))
}

/// Parse the vmnet DHCP leases text and return the IP for the given MAC.
///
/// The MAC is matched case-insensitively against the `hw_address` field,
/// ignoring the `1,` hardware-type prefix.
fn find_ip_in_dhcp_leases(leases_text: &str, mac: &str) -> Option<String> {
    let normalized_target = normalize_mac(mac);

    // Parse brace-delimited lease entries.
    let mut ip: Option<String> = None;
    let mut hw_mac = None;

    for line in leases_text.lines() {
        let line = line.trim();

        if line == "{" {
            // Start of a new lease entry — reset state.
            ip = None;
            hw_mac = None;
        } else if line == "}" {
            // End of entry — check for match.
            if let (Some(lease_ip), Some(lease_mac)) = (&ip, &hw_mac) {
                if lease_mac == &normalized_target {
                    return Some(lease_ip.clone());
                }
            }
        } else if let Some(value) = line.strip_prefix("ip_address=") {
            ip = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("hw_address=") {
            // Strip the hardware-type prefix (e.g. "1," for Ethernet).
            // Normalize to handle macOS stripping leading zeros from octets
            // (e.g. "1,22:bc:4d:71:d6:6" → "22:bc:4d:71:d6:06").
            let mac_part = value.split_once(',').map(|(_, m)| m).unwrap_or(value);
            hw_mac = Some(normalize_mac(mac_part));
        }
    }

    None
}

/// Parse `arp -a` output and return the IP for the given MAC.
///
/// macOS `arp -a` output format:
/// ```text
/// ? (192.168.64.3) at ca:8a:b2:b8:2b:af on bridge100 ifscope [ethernet]
/// hostname (192.168.0.1) at 90:5c:44:55:9f:c8 on en0 ifscope [ethernet]
/// ```
///
/// Note: macOS arp omits leading zeros in MAC octets (e.g. `e:49:2d:...`
/// instead of `0e:49:2d:...`). We normalize both MACs before comparing.
fn find_ip_in_arp_output(arp_output: &str, mac: &str) -> Option<String> {
    let normalized_target = normalize_mac(mac);

    for line in arp_output.lines() {
        // Format: "hostname (IP) at MAC on IFACE ..."
        // The MAC field comes after " at " and before " on ".
        let after_at = line.split(" at ").nth(1)?;
        let arp_mac = after_at.split(" on ").next()?;

        if arp_mac == "(incomplete)" {
            continue;
        }

        if normalize_mac(arp_mac) == normalized_target {
            // Extract IP from between parentheses: "? (192.168.64.3) at ..."
            let start = line.find('(')? + 1;
            let end = line.find(')')?;
            return Some(line[start..end].to_string());
        }
    }
    None
}

/// Normalize a MAC address to lowercase with zero-padded octets.
///
/// Handles macOS arp's shorthand (e.g. `e:49:2d:ed:bf:e5` → `0e:49:2d:ed:bf:e5`).
fn normalize_mac(mac: &str) -> String {
    mac.to_lowercase()
        .split(':')
        .map(|octet| format!("{:0>2}", octet))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LEASES: &str = "\
{
\tip_address=192.168.64.3
\thw_address=1,ca:8a:b2:b8:2b:af
\tidentifier=1,ca:8a:b2:b8:2b:af
\tlease=0x6734d688
}
{
\tname=nixos
\tip_address=192.168.64.2
\thw_address=1,6a:ad:4c:41:03:42
\tidentifier=1,6a:ad:4c:41:03:42
\tlease=0x624af19f
}
";

    #[test]
    fn find_existing_mac() {
        let ip = find_ip_in_dhcp_leases(SAMPLE_LEASES, "ca:8a:b2:b8:2b:af");
        assert_eq!(ip, Some("192.168.64.3".to_string()));
    }

    #[test]
    fn find_second_entry() {
        let ip = find_ip_in_dhcp_leases(SAMPLE_LEASES, "6a:ad:4c:41:03:42");
        assert_eq!(ip, Some("192.168.64.2".to_string()));
    }

    #[test]
    fn case_insensitive_match() {
        let ip = find_ip_in_dhcp_leases(SAMPLE_LEASES, "CA:8A:B2:B8:2B:AF");
        assert_eq!(ip, Some("192.168.64.3".to_string()));
    }

    #[test]
    fn unknown_mac_returns_none() {
        let ip = find_ip_in_dhcp_leases(SAMPLE_LEASES, "00:00:00:00:00:00");
        assert_eq!(ip, None);
    }

    #[test]
    fn empty_leases_returns_none() {
        let ip = find_ip_in_dhcp_leases("", "ca:8a:b2:b8:2b:af");
        assert_eq!(ip, None);
    }

    #[test]
    fn dhcp_short_octets_match() {
        // macOS DHCP leases strip leading zeros from MAC octets
        // (e.g. "22:bc:4d:71:d6:6" instead of "22:bc:4d:71:d6:06").
        let leases = "\
{
\tip_address=192.168.64.9
\thw_address=1,22:bc:4d:71:d6:6
\tidentifier=1,22:bc:4d:71:d6:6
\tlease=0x69aefd42
}
";
        let ip = find_ip_in_dhcp_leases(leases, "22:bc:4d:71:d6:06");
        assert_eq!(ip, Some("192.168.64.9".to_string()));
    }

    // ── ARP table parsing tests ──────────────────────────────────

    const SAMPLE_ARP: &str = "\
? (192.168.64.3) at ca:8a:b2:b8:2b:af on bridge100 ifscope [ethernet]
compalhub.home (192.168.0.1) at 90:5c:44:55:9f:c8 on en0 ifscope [ethernet]
? (192.168.64.5) at e:49:2d:ed:bf:e5 on bridge100 ifscope [ethernet]
? (169.254.169.254) at (incomplete) on en0 [ethernet]";

    #[test]
    fn arp_find_existing_mac() {
        let ip = find_ip_in_arp_output(SAMPLE_ARP, "ca:8a:b2:b8:2b:af");
        assert_eq!(ip, Some("192.168.64.3".to_string()));
    }

    #[test]
    fn arp_find_with_short_octets() {
        // macOS arp shows "e:49:2d:..." but our MAC may have "0e:49:2d:..."
        let ip = find_ip_in_arp_output(SAMPLE_ARP, "0e:49:2d:ed:bf:e5");
        assert_eq!(ip, Some("192.168.64.5".to_string()));
    }

    #[test]
    fn arp_case_insensitive() {
        let ip = find_ip_in_arp_output(SAMPLE_ARP, "CA:8A:B2:B8:2B:AF");
        assert_eq!(ip, Some("192.168.64.3".to_string()));
    }

    #[test]
    fn arp_skips_incomplete() {
        let ip = find_ip_in_arp_output(SAMPLE_ARP, "(incomplete)");
        assert_eq!(ip, None);
    }

    #[test]
    fn arp_unknown_mac_returns_none() {
        let ip = find_ip_in_arp_output(SAMPLE_ARP, "00:00:00:00:00:00");
        assert_eq!(ip, None);
    }

    // ── MAC normalization tests ──────────────────────────────────

    #[test]
    fn normalize_pads_short_octets() {
        assert_eq!(normalize_mac("e:49:2d:ed:bf:e5"), "0e:49:2d:ed:bf:e5");
    }

    #[test]
    fn normalize_already_padded() {
        assert_eq!(normalize_mac("ca:8a:b2:b8:2b:af"), "ca:8a:b2:b8:2b:af");
    }

    #[test]
    fn normalize_uppercase() {
        assert_eq!(normalize_mac("CA:8A:B2:B8:2B:AF"), "ca:8a:b2:b8:2b:af");
    }

    // ── WAN interface detection (route parser) ───────────────────

    #[test]
    fn parse_route_typical_output() {
        let output = "\
   route to: dns.google
destination: default
       mask: default
    gateway: 192.168.0.1
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>
 recvpipe  sendpipe  ssthresh  rtt,msec    rttvar  hopcount      mtu     expire
       0         0         0         0         0         0      1500         0
";
        assert_eq!(parse_interface_from_route(output), Some("en0".to_string()));
    }

    #[test]
    fn parse_route_wifi_interface() {
        let output = "  interface: en1\n";
        assert_eq!(parse_interface_from_route(output), Some("en1".to_string()));
    }

    #[test]
    fn parse_route_no_interface_line() {
        let output = "route to: dns.google\ndestination: default\n";
        assert_eq!(parse_interface_from_route(output), None);
    }

    #[test]
    fn parse_route_empty() {
        assert_eq!(parse_interface_from_route(""), None);
    }
}
