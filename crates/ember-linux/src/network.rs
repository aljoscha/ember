pub mod dns;
pub mod ip;
pub mod iptables;
pub mod nat;
pub mod policy;
pub mod tap;
pub mod wan;

use ember_core::config::GlobalConfig;
use ember_core::state::store::StateStore;
use ember_core::state::vm::NetworkInfo;

/// Best-effort cleanup of networking resources for a VM (Linux only).
///
/// Rules are removed in the shape they were added: `net_info` records
/// both the WAN interface captured at start and the chain the VM's
/// forwarding rules went into, so a default-route change or a binary
/// upgrade between start and stop cannot turn the `-D` calls into
/// silent no-ops. The iptables comment comes from [`nat::comment`] so
/// deletions in shared chains only ever match this installation's
/// rules.
pub fn cleanup(store: &StateStore, config: &GlobalConfig, vm_name: &str, net_info: &NetworkInfo) {
    let wan_iface = net_info.wan_iface.clone().or_else(|| wan::detect().ok());
    if let Some(wan_iface) = wan_iface {
        nat::VmRules {
            chain: net_info.firewall_chain.as_deref(),
            tap_device: &net_info.tap_device,
            guest_ip: &net_info.guest_ip,
            wan_iface: &wan_iface,
            comment: &nat::comment(config.instance_namespace()),
        }
        .remove();
    }
    let _ = tap::delete(&net_info.tap_device);
    let _ = ip::release(store, vm_name);
}
