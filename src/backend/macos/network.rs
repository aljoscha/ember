//! macOS network backend: vmnet shared mode.
//!
//! vmnet provides NAT + DHCP automatically — most operations are no-ops.
//! The main work is discovering the guest IP from DHCP leases after boot.

use std::path::PathBuf;

use crate::backend::NetworkBackend;
use crate::cli::init::GlobalConfig;
use crate::error::{Error, Result};
use crate::state::vm::{NetworkInfo, VmMetadata};

/// Default gateway for vmnet shared mode (192.168.64.0/24 network).
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

    /// Discover the guest IP by looking up its MAC address in the vmnet
    /// DHCP lease file (`/var/db/dhcpd_leases`).
    ///
    /// The lease file contains brace-delimited entries like:
    /// ```text
    /// {
    ///     ip_address=192.168.64.3
    ///     hw_address=1,ca:8a:b2:b8:2b:af
    ///     ...
    /// }
    /// ```
    /// The `hw_address` has a hardware-type prefix (`1,` for Ethernet)
    /// followed by the MAC in colon-separated hex. We match against the
    /// MAC portion, case-insensitively.
    fn discover_guest_ip(&self, mac: &str) -> Result<String> {
        let lease_path = PathBuf::from(DHCP_LEASES_PATH);
        let contents = std::fs::read_to_string(&lease_path).map_err(|e| Error::Io {
            path: lease_path,
            source: e,
        })?;

        find_ip_for_mac(&contents, mac).ok_or_else(|| {
            Error::Network(format!(
                "no DHCP lease found for MAC {mac} in {DHCP_LEASES_PATH}\n\
                 Hint: the VM may not have obtained an IP yet"
            ))
        })
    }
}

/// Parse the vmnet DHCP leases text and return the IP for the given MAC.
///
/// The MAC is matched case-insensitively against the `hw_address` field,
/// ignoring the `1,` hardware-type prefix.
fn find_ip_for_mac(leases_text: &str, mac: &str) -> Option<String> {
    let mac_lower = mac.to_lowercase();

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
                if lease_mac == &mac_lower {
                    return Some(lease_ip.clone());
                }
            }
        } else if let Some(value) = line.strip_prefix("ip_address=") {
            ip = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("hw_address=") {
            // Strip the hardware-type prefix (e.g. "1," for Ethernet).
            let mac_part = value.split_once(',').map(|(_, m)| m).unwrap_or(value);
            hw_mac = Some(mac_part.to_lowercase());
        }
    }

    None
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
        let ip = find_ip_for_mac(SAMPLE_LEASES, "ca:8a:b2:b8:2b:af");
        assert_eq!(ip, Some("192.168.64.3".to_string()));
    }

    #[test]
    fn find_second_entry() {
        let ip = find_ip_for_mac(SAMPLE_LEASES, "6a:ad:4c:41:03:42");
        assert_eq!(ip, Some("192.168.64.2".to_string()));
    }

    #[test]
    fn case_insensitive_match() {
        let ip = find_ip_for_mac(SAMPLE_LEASES, "CA:8A:B2:B8:2B:AF");
        assert_eq!(ip, Some("192.168.64.3".to_string()));
    }

    #[test]
    fn unknown_mac_returns_none() {
        let ip = find_ip_for_mac(SAMPLE_LEASES, "00:00:00:00:00:00");
        assert_eq!(ip, None);
    }

    #[test]
    fn empty_leases_returns_none() {
        let ip = find_ip_for_mac("", "ca:8a:b2:b8:2b:af");
        assert_eq!(ip, None);
    }
}
