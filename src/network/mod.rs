pub mod dns;
pub mod ip;
pub mod nat;
pub mod tap;
pub mod wan;

use crate::state::store::StateStore;
use crate::state::vm::NetworkInfo;

/// Best-effort cleanup of networking resources for a VM.
///
/// Removes iptables NAT/forwarding rules, deletes the TAP device, and
/// releases the IP allocation. Errors are silently ignored since this
/// is called during cleanup paths where partial failure is acceptable.
pub fn cleanup(store: &StateStore, vm_name: &str, net_info: &NetworkInfo) {
    // Use the stored WAN interface (matches what was used to create the rules),
    // falling back to re-detection for backwards compatibility with older metadata.
    let wan_iface = net_info
        .wan_iface
        .clone()
        .or_else(|| wan::detect().ok());
    if let Some(wan_iface) = wan_iface {
        let _ = nat::remove_rules(&net_info.tap_device, &net_info.guest_ip, &wan_iface);
    }
    let _ = tap::delete(&net_info.tap_device);
    let _ = ip::release(store, vm_name);
}
