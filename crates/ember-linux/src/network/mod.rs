pub use ember_core::network::ip;

pub mod dns;
pub mod nat;
pub mod tap;
pub mod wan;

use ember_core::state::store::StateStore;
use ember_core::state::vm::NetworkInfo;

/// Best-effort cleanup of networking resources for a VM (Linux only).
pub fn cleanup(store: &StateStore, vm_name: &str, net_info: &NetworkInfo) {
    let wan_iface = net_info.wan_iface.clone().or_else(|| wan::detect().ok());
    if let Some(wan_iface) = wan_iface {
        let _ = nat::remove_rules(&net_info.tap_device, &net_info.guest_ip, &wan_iface);
    }
    let _ = tap::delete(&net_info.tap_device);
    let _ = ip::release(store, vm_name);
}
