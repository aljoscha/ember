//! macOS network backend: vmnet shared mode with static IP allocation.
//!
//! vmnet provides NAT automatically via `VZNATNetworkDeviceAttachment`.
//! Guest IPs are statically allocated from the vmnet subnet and passed
//! to the kernel via the `ip=` boot parameter — no DHCP dependency.

use std::process::Command;

use ember_core::backend::NetworkBackend;
use ember_core::config::GlobalConfig;
use ember_core::error::{Error, Result};
use ember_core::network;
use ember_core::state::store::StateStore;
use ember_core::state::vm::{NetworkInfo, VmMetadata};

/// vmnet shared mode gateway.
pub const VMNET_GATEWAY: &str = "192.168.64.1";

/// Default netmask for vmnet shared mode (/24).
pub const VMNET_NETMASK: &str = "255.255.255.0";

/// Host-wide vmnet shared-mode subnet. Apple's `VZNATNetworkDevice`
/// owns this /24 and there's no public API to ask for a different
/// one, so per-installation isolation can only sub-allocate inside
/// it; see [`derive_vmnet_subnet`].
///
/// Used directly only as the legacy fallback for configs that
/// predate `instance_id` — new installs read
/// [`GlobalConfig::ip_subnet`](ember_core::config::GlobalConfig::ip_subnet)
/// (a /27 slice) instead.
pub const VMNET_SUBNET: &str = "192.168.64.0/24";

/// vmnet-owned addresses that no guest may receive, regardless of
/// which /27 slot an install lands in: the surrounding /24's network
/// (.0) and broadcast (.255), plus vmnet's built-in router (.1).
/// Addresses outside the install's /27 slot are still listed; the
/// allocator just ignores reservations outside the subnet it's
/// walking, so this single list covers slots 0, 7, and the middle
/// six identically.
const VMNET_RESERVED: [std::net::Ipv4Addr; 3] = [
    std::net::Ipv4Addr::new(192, 168, 64, 0),
    std::net::Ipv4Addr::new(192, 168, 64, 1),
    std::net::Ipv4Addr::new(192, 168, 64, 255),
];

/// Derive a per-installation /27 sub-range inside [`VMNET_SUBNET`].
///
/// vmnet's shared subnet is fixed by the framework, so isolation has
/// to come from carving up the /24 rather than picking a different
/// one. We split it into 8 /27 slots (32 addresses each) and pick by
/// the low 3 bits of the instance id parsed as hex. The collision
/// probability between two installs is 1/8 per pair — acceptable for
/// personal use, with `--ip-subnet` as the escape hatch when it
/// bites.
///
/// Pairs with [`network::ip::allocate_single`](ember_core::network::ip::allocate_single):
/// each /27 slot holds 30–32 single-IP allocations after subtracting
/// vmnet's reserved network/gateway/broadcast addresses, well above
/// any realistic personal workflow.
pub fn derive_vmnet_subnet(instance_id: &str) -> String {
    let id_int = u16::from_str_radix(instance_id, 16).unwrap_or(0);
    let slot = (id_int & 0b111) as u8;
    let base = slot * 32;
    format!("192.168.64.{base}/27")
}

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
    /// so the guest has connectivity immediately at boot — no DHCP
    /// needed.
    ///
    /// Two allocation strategies, picked by whether the config has
    /// an `instance_id`:
    ///
    /// * Legacy (no instance id): `network::ip::allocate` against the
    ///   full /24, matching what pre-instance-id binaries wrote so an
    ///   upgrade doesn't reinterpret existing `allocations.json`
    ///   block indices. Caps at ~64 VMs (one /30 per VM).
    /// * New install: `network::ip::allocate_single` against the
    ///   per-installation /27 slice. Single-IP allocation gives ~30
    ///   VMs per slot — vmnet's shared L2 bridge means the /30 P2P
    ///   link Linux needs is overkill here and would waste 75% of
    ///   the address space.
    fn setup(&self, vm: &VmMetadata, config: &GlobalConfig) -> Result<NetworkInfo> {
        let allocation = if config.instance_id.is_empty() {
            // Per-VM `vm.subnet` overrides the install default; keeps
            // parity with pre-instance-id behavior.
            let subnet = vm.subnet.as_deref().unwrap_or(VMNET_SUBNET);
            network::ip::allocate(&self.store, subnet, &vm.name)?
        } else {
            let subnet = vm.subnet.as_deref().unwrap_or(config.ip_subnet.as_str());
            network::ip::allocate_single(
                &self.store,
                subnet,
                &vm.name,
                VMNET_GATEWAY,
                VMNET_NETMASK,
                &VMNET_RESERVED,
            )?
        };

        Ok(NetworkInfo {
            tap_device: String::new(),
            host_ip: VMNET_GATEWAY.to_string(),
            guest_ip: allocation.guest_ip,
            netmask: VMNET_NETMASK.to_string(),
            guest_mac: None,
            wan_iface: None,
        })
    }

    fn teardown(&self, vm: &VmMetadata, _config: &GlobalConfig) -> Result<()> {
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

    // ── vmnet sub-range derivation ───────────────────────────────

    #[test]
    fn vmnet_subnet_lands_in_192_168_64_slash_24() {
        for id in ["0000", "a3f4", "ffff", "dead", "beef"] {
            let subnet = derive_vmnet_subnet(id);
            assert!(
                subnet.starts_with("192.168.64."),
                "instance id {id} produced subnet outside vmnet's /24: {subnet}"
            );
            assert!(
                subnet.ends_with("/27"),
                "instance id {id} produced non-/27 subnet: {subnet}"
            );
        }
    }

    #[test]
    fn vmnet_subnet_uses_low_three_bits_of_id() {
        // Same low 3 bits → same slot; bits 3-15 don't move it.
        // 0x0007 and 0xffff both end in 0b111 → slot 7 → base 224.
        assert_eq!(derive_vmnet_subnet("0007"), "192.168.64.224/27");
        assert_eq!(derive_vmnet_subnet("ffff"), "192.168.64.224/27");
        // 0x0000 → slot 0 → base 0.
        assert_eq!(derive_vmnet_subnet("0000"), "192.168.64.0/27");
        // 0x0008 → slot 0 → base 0 (only low 3 bits matter).
        assert_eq!(derive_vmnet_subnet("0008"), "192.168.64.0/27");
    }

    #[test]
    fn vmnet_subnet_is_stable() {
        // Reactivation must land on the same /27 the install was
        // initialized with, otherwise allocations.json's pinned
        // `base_subnet` mismatches and `network::ip::allocate`
        // refuses to hand out IPs.
        assert_eq!(derive_vmnet_subnet("a3f4"), derive_vmnet_subnet("a3f4"));
    }

    #[test]
    fn vmnet_subnet_falls_back_to_slot_zero_on_garbage_id() {
        // Defensive: parse failure shouldn't panic, just land on
        // the legacy-equivalent base. The user can then pick
        // `--ip-subnet` explicitly.
        assert_eq!(derive_vmnet_subnet("zzzz"), "192.168.64.0/27");
        assert_eq!(derive_vmnet_subnet(""), "192.168.64.0/27");
    }
}
