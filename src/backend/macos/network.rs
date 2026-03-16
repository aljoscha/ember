//! macOS network backend: vmnet shared mode with static IP allocation.
//!
//! vmnet provides NAT automatically via `VZNATNetworkDeviceAttachment`.
//! Guest IPs are statically allocated from the vmnet subnet and passed
//! to the kernel via the `ip=` boot parameter — no DHCP dependency.

use std::process::Command;

use crate::backend::NetworkBackend;
use crate::config::GlobalConfig;
use crate::error::{Error, Result};
use crate::network;
use crate::state::store::StateStore;
use crate::state::vm::{NetworkInfo, VmMetadata};

/// vmnet shared mode gateway.
pub const VMNET_GATEWAY: &str = "192.168.64.1";

/// Default netmask for vmnet shared mode (/24).
pub const VMNET_NETMASK: &str = "255.255.255.0";

/// vmnet shared mode subnet for IP allocation.
///
/// Uses /30 blocks via the shared `network::ip` allocator, giving 64
/// concurrent VMs. The gateway (.1) is always vmnet's built-in router.
const VMNET_SUBNET: &str = "192.168.64.0/24";

/// macOS network backend using vmnet (shared mode).
///
/// Allocates static guest IPs from the vmnet subnet. vmnet handles
/// NAT internally — no TAP devices or firewall rules needed.
pub struct MacosNetwork {
    store: StateStore,
}

impl MacosNetwork {
    pub fn new(store: StateStore) -> Self {
        Self { store }
    }
}

impl NetworkBackend for MacosNetwork {
    /// Allocate a static guest IP from the vmnet subnet.
    ///
    /// The IP is passed to the kernel via boot args (`ip=<guest>::...`)
    /// so the guest has connectivity immediately at boot — no DHCP needed.
    fn setup(&self, vm: &VmMetadata, _config: &GlobalConfig) -> Result<NetworkInfo> {
        let allocation = network::ip::allocate(&self.store, VMNET_SUBNET, &vm.name)?;

        Ok(NetworkInfo {
            tap_device: String::new(),
            host_ip: VMNET_GATEWAY.to_string(),
            guest_ip: allocation.guest_ip,
            netmask: VMNET_NETMASK.to_string(),
            guest_mac: None,
            wan_iface: None,
        })
    }

    fn teardown(&self, vm: &VmMetadata) -> Result<()> {
        let _ = network::ip::release(&self.store, &vm.name);
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
