pub use ember_core::network::ip;

#[cfg(target_os = "linux")]
pub mod dns;
#[cfg(target_os = "linux")]
pub mod nat;
#[cfg(target_os = "linux")]
pub mod tap;
#[cfg(target_os = "linux")]
pub mod wan;

#[cfg(target_os = "linux")]
use crate::state::store::StateStore;
#[cfg(target_os = "linux")]
use crate::state::vm::NetworkInfo;

/// Best-effort cleanup of networking resources for a VM (Linux only).
#[cfg(target_os = "linux")]
pub fn cleanup(store: &StateStore, vm_name: &str, net_info: &NetworkInfo) {
    let wan_iface = net_info.wan_iface.clone().or_else(|| wan::detect().ok());
    if let Some(wan_iface) = wan_iface {
        let _ = nat::remove_rules(&net_info.tap_device, &net_info.guest_ip, &wan_iface);
    }
    let _ = tap::delete(&net_info.tap_device);
    let _ = ip::release(store, vm_name);
}
