//! Lightweight state reconciliation for crash recovery.
//!
//! Called on every command invocation to clean up after crashes:
//!
//! 1. For each VM in Running/Paused state, check if the Firecracker PID
//!    is still alive. If dead, mark the VM as Stopped and clean up its
//!    network resources (TAP device, iptables rules, IP allocation).
//!
//! 2. Find orphaned TAP devices belonging to *this* installation
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

    // Phase 2: Clean up orphaned TAP devices belonging to this install.
    // Without a config we have no way to scope the listing safely, so
    // skip — leaving an orphan is preferable to deleting a foreign one.
    let Some(cfg) = config else {
        return;
    };
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
