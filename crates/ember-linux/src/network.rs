pub mod dns;
pub mod ip;
pub mod nat;
pub mod tap;
pub mod wan;

use ember_core::config::GlobalConfig;
use ember_core::state::store::StateStore;
use ember_core::state::vm::NetworkInfo;

/// Best-effort cleanup of networking resources for a VM (Linux only).
///
/// The iptables comment is derived via [`nat::comment`] from the
/// install's namespace so the `-D` calls only match this
/// installation's rules even when another ember install on the same
/// host has rules for the same TAP/IP.
pub fn cleanup(store: &StateStore, config: &GlobalConfig, vm_name: &str, net_info: &NetworkInfo) {
    let wan_iface = net_info.wan_iface.clone().or_else(|| wan::detect().ok());
    if let Some(wan_iface) = wan_iface {
        let _ = nat::remove_rules(
            &net_info.tap_device,
            &net_info.guest_ip,
            &wan_iface,
            &nat::comment(config.instance_namespace()),
        );
    }
    let _ = tap::delete(&net_info.tap_device);
    let _ = ip::release(store, vm_name);
}
