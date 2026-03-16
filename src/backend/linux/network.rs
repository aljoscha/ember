//! Linux network backend: TAP devices, iptables NAT, and IP allocation.
//!
//! Wraps the `network::ip`, `network::tap`, `network::nat`, and `network::wan`
//! modules behind the [`NetworkBackend`] trait. On Linux, each VM gets a
//! dedicated TAP device with a point-to-point /30 IP link and iptables
//! masquerade rules for outbound internet access.

use crate::backend::NetworkBackend;
use crate::config::GlobalConfig;
use crate::error::Result;
use crate::network;
use crate::state::store::StateStore;
use crate::state::vm::{NetworkInfo, VmMetadata};

/// Linux network backend using TAP devices and iptables.
pub struct LinuxNetwork {
    /// State store for IP allocation tracking.
    store: StateStore,
}

impl LinuxNetwork {
    /// Create a new Linux network backend.
    pub fn new(store: StateStore) -> Self {
        Self { store }
    }
}

impl NetworkBackend for LinuxNetwork {
    /// Set up networking: allocate IP, create TAP device, add iptables rules.
    ///
    /// Returns the [`NetworkInfo`] with the allocated addresses, TAP device
    /// name, and WAN interface used for NAT. This info is stored in VM
    /// metadata and used for cleanup during teardown.
    fn setup(&self, vm: &VmMetadata, config: &GlobalConfig) -> Result<NetworkInfo> {
        // Determine WAN interface (from config or auto-detect).
        let wan_iface = match &config.wan_iface {
            Some(iface) => iface.clone(),
            None => network::wan::detect()?,
        };

        // Allocate a /30 IP block for this VM.
        let subnet = vm.subnet.as_deref().unwrap_or(network::ip::DEFAULT_SUBNET);
        let allocation = network::ip::allocate(&self.store, subnet, &vm.name)?;

        // Create TAP device.
        let tap_name = network::tap::device_name(&vm.id);
        let host_ip_cidr = format!("{}/30", allocation.host_ip);
        if let Err(e) = network::tap::create(&tap_name, &host_ip_cidr) {
            // Clean up IP allocation on failure.
            let _ = network::ip::release(&self.store, &vm.name);
            return Err(e);
        }

        // Enable IP forwarding (idempotent).
        if let Err(e) = network::nat::enable_ip_forwarding() {
            let _ = network::tap::delete(&tap_name);
            let _ = network::ip::release(&self.store, &vm.name);
            return Err(e);
        }

        // Add iptables NAT/forwarding rules.
        if let Err(e) = network::nat::add_rules(&tap_name, &allocation.guest_ip, &wan_iface) {
            let _ = network::tap::delete(&tap_name);
            let _ = network::ip::release(&self.store, &vm.name);
            return Err(e);
        }

        Ok(NetworkInfo {
            tap_device: tap_name,
            host_ip: allocation.host_ip,
            guest_ip: allocation.guest_ip,
            netmask: allocation.netmask,
            guest_mac: None,
            wan_iface: Some(wan_iface),
        })
    }

    /// Tear down networking: remove iptables rules, delete TAP device, release IP.
    ///
    /// Best-effort cleanup — continues even if individual steps fail, since
    /// this is called during stop/delete where partial cleanup is acceptable.
    fn teardown(&self, vm: &VmMetadata) -> Result<()> {
        if let Some(ref net_info) = vm.network {
            network::cleanup(&self.store, &vm.name, net_info);
        }
        Ok(())
    }
}
