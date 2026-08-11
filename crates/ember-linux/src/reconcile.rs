//! Lightweight state reconciliation for crash recovery.
//!
//! Called on every command invocation to clean up after crashes:
//!
//! 1. For each VM in Running/Paused state, check if the Firecracker PID
//!    is still alive. If dead, mark the VM as Stopped and clean up its
//!    network resources (TAP device, iptables rules, IP allocation).
//!
//! 2. Move the forwarding rules of VMs that were started before the
//!    installation had policy chains into the chain, so the chain's
//!    terminal DROP doesn't cut them off.
//!
//! 3. Find orphaned TAP devices belonging to *this* installation
//!    (matched against [`network::tap::prefix`] for the install's
//!    namespace) and delete them. Other ember installs use distinct
//!    prefixes, so reconciliation here never touches their devices.
//!
//! All operations are best-effort: errors are logged but never propagated,
//! since reconciliation should not block normal CLI operation.

use std::collections::HashSet;
use std::path::Path;

use crate::firecracker;
use crate::network;
use ember_core::config::GlobalConfig;
use ember_core::state::store::StateStore;
use ember_core::state::vm::{self, VmStatus};

/// Run lightweight state reconciliation.
///
/// Should be called early in command dispatch, before any VM operation.
/// Catches and logs all errors internally — never returns `Err`.
pub fn run(state_dir: &Path) {
    let store = match StateStore::try_open(state_dir) {
        Some(s) => s,
        None => return, // State dir doesn't exist yet (pre-init), nothing to reconcile.
    };

    // Need the global config for the per-installation TAP prefix and
    // iptables comment. If it's missing or unreadable, reconcile
    // per-VM state but skip the prefix-based TAP sweep — without a
    // prefix we'd risk deleting another install's devices.
    let config: Option<GlobalConfig> = store.read_optional(&store.config_path()).ok().flatten();

    let vms = match vm::list(&store) {
        Ok(vms) => vms,
        Err(e) => {
            eprintln!("Warning: reconciliation failed to list VMs: {e}");
            return;
        }
    };

    // Track TAP devices that belong to legitimately running VMs.
    let mut active_tap_devices = HashSet::new();
    // Running VMs whose forwarding rules predate the policy chains.
    let mut unchained = Vec::new();

    // Phase 1: Reconcile VMs whose processes have died.
    for metadata in vms {
        match metadata.status {
            VmStatus::Running | VmStatus::Paused => {}
            _ => {
                // Not running — nothing to check.
                continue;
            }
        }

        let pid = match metadata.pid {
            Some(pid) => pid,
            None => {
                // Running/Paused but no PID — state is corrupt. Mark stopped.
                eprintln!(
                    "Warning: VM '{}' is {} but has no PID, marking stopped",
                    metadata.name, metadata.status
                );
                mark_stopped(&store, &metadata);
                continue;
            }
        };

        if firecracker::process::is_alive(pid) {
            // Process is alive — this VM is genuinely running.
            if let Some(ref net) = metadata.network {
                active_tap_devices.insert(net.tap_device.clone());
                if net.firewall_chain.is_none() {
                    unchained.push(metadata.clone());
                }
            }
        } else {
            // Process is dead — clean up and mark stopped.
            eprintln!(
                "Warning: VM '{}' process (pid {pid}) is dead, marking stopped",
                metadata.name
            );
            if let Some(ref net_info) = metadata.network {
                if let Some(ref cfg) = config {
                    network::cleanup(&store, cfg, &metadata.name, net_info);
                }
            }
            mark_stopped(&store, &metadata);
        }
    }

    // Without a config we have no way to scope host-global names safely,
    // so skip the rest — leaving an orphan is preferable to deleting a
    // foreign one.
    let Some(cfg) = config else {
        return;
    };

    // Phase 2: Adopt running VMs whose rules predate the policy chains.
    for metadata in unchained {
        adopt_into_policy_chain(&store, &cfg, &metadata);
    }

    // Phase 3: Clean up orphaned TAP devices belonging to this install.
    let prefix = network::tap::prefix(cfg.instance_namespace());
    let system_devices = match network::tap::list_devices_with_prefix(&prefix) {
        Ok(devs) => devs,
        Err(e) => {
            eprintln!("Warning: failed to list TAP devices: {e}");
            return;
        }
    };

    for device in system_devices {
        if !active_tap_devices.contains(&device) {
            eprintln!("Warning: deleting orphaned TAP device '{device}'");
            let _ = network::tap::delete(&device);
        }
    }
}

/// Move a running VM's forwarding rules into the install's policy
/// chain.
///
/// A VM started before the install had policy chains has its rules
/// appended to the built-in FORWARD chain, which is below the jump into
/// our chain, so the chain's terminal DROP would cut the VM off the
/// moment the chain appears. Rather than leave the user with a live VM
/// that has silently lost its network until they restart it, we re-add
/// its rules inside the chain and delete the ones outside.
///
/// The masquerade rule is deliberately untouched. Its shape is
/// identical in both modes, so adding and then removing the full set
/// would delete it and break the VM's outbound NAT.
///
/// Best effort. On failure the VM keeps working exactly as badly as it
/// would have anyway, and the warning says what to do about it.
fn adopt_into_policy_chain(store: &StateStore, config: &GlobalConfig, metadata: &vm::VmMetadata) {
    let Some(net) = metadata.network.as_ref() else {
        return;
    };
    let Some(wan_iface) = net
        .wan_iface
        .clone()
        .or_else(|| network::wan::detect().ok())
    else {
        return;
    };

    let ns = config.instance_namespace();
    if let Err(e) = network::policy::ensure(ns) {
        eprintln!("Warning: could not set up firewall chains: {e}");
        return;
    }
    let chains = network::policy::chains(ns);

    let comment = network::nat::comment(ns);
    let in_chain = network::nat::VmRules {
        chain: Some(&chains.forward),
        tap_device: &net.tap_device,
        guest_ip: &net.guest_ip,
        wan_iface: &wan_iface,
        comment: &comment,
    };
    let outside = network::nat::VmRules {
        chain: None,
        ..in_chain
    };

    // Add before removing, so the VM is never without a rule. A brief
    // duplicate ACCEPT is harmless.
    if let Err(e) = in_chain.add_forwarding() {
        eprintln!(
            "Warning: VM '{}' still has its firewall rules outside '{}' \
             and may have lost network access ({e}). Restart it with \
             'ember vm stop {} && ember vm start {}'.",
            metadata.name, chains.forward, metadata.name, metadata.name
        );
        return;
    }
    outside.remove_forwarding();

    // Record where the rules now live, so teardown deletes them from
    // the chain rather than from the built-in one.
    let result = vm::update(store, &metadata.name, |m| {
        if let Some(ref mut net) = m.network {
            net.firewall_chain = Some(chains.forward.clone());
        }
        Ok(())
    });
    if let Err(e) = result {
        eprintln!(
            "Warning: failed to record the firewall chain for VM '{}': {e}",
            metadata.name
        );
    }
}

/// Mark a VM as Stopped, clearing its PID and network info.
fn mark_stopped(store: &StateStore, metadata: &vm::VmMetadata) {
    let result = vm::update(store, &metadata.name, |m| {
        m.status = VmStatus::Stopped;
        m.pid = None;
        m.network = None;
        Ok(())
    });
    if let Err(e) = result {
        eprintln!(
            "Warning: failed to update VM '{}' state: {e}",
            metadata.name
        );
    }
}
