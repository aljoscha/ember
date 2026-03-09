//! macOS network backend: vmnet shared mode.
//!
//! vmnet provides NAT + DHCP automatically — most operations are no-ops.
//! The main work is discovering the guest IP from DHCP leases after boot.

use crate::backend::NetworkBackend;
use crate::cli::init::GlobalConfig;
use crate::error::Result;
use crate::state::vm::{NetworkInfo, VmMetadata};

/// Default gateway for vmnet shared mode (192.168.64.0/24 network).
const VMNET_GATEWAY: &str = "192.168.64.1";

/// Default netmask for vmnet shared mode (/24).
const VMNET_NETMASK: &str = "255.255.255.0";

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

    fn discover_guest_ip(&self, _mac: &str) -> Result<String> {
        todo!("macOS: discover guest IP from DHCP leases")
    }
}
