//! Lightweight state reconciliation for crash recovery.
//!
//! Called on every command invocation to clean up after crashes:
//!
//! 1. For each VM in Running/Paused state, check if the Firecracker PID
//!    is still alive. If dead, mark the VM as Stopped and clean up its
//!    network resources (TAP device, iptables rules, IP allocation).
//!
//! 2. Find orphaned `em-*` TAP devices (present on the system but not
//!    associated with any running VM) and delete them.
//!
//! All operations are best-effort: errors are logged but never propagated,
//! since reconciliation should not block normal CLI operation.

use std::collections::HashSet;
use std::path::Path;

use crate::firecracker;
use crate::network;
use crate::state::store::StateStore;
use crate::state::vm::{self, VmStatus};

/// Run lightweight state reconciliation.
///
/// Should be called early in command dispatch, before any VM operation.
/// Catches and logs all errors internally — never returns `Err`.
pub fn run(state_dir: &Path) {
    let store = match StateStore::try_open(state_dir) {
        Some(s) => s,
        None => return, // State dir doesn't exist yet (pre-init), nothing to reconcile.
    };

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
    for mut metadata in vms {
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
                mark_stopped(&store, &mut metadata);
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
                cleanup_network(&store, &metadata.name, net_info);
            }
            mark_stopped(&store, &mut metadata);
        }
    }

    // Phase 2: Clean up orphaned TAP devices.
    let system_devices = match network::tap::list_ember_devices() {
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
fn mark_stopped(store: &StateStore, metadata: &mut vm::VmMetadata) {
    metadata.status = VmStatus::Stopped;
    metadata.pid = None;
    metadata.network = None;
    if let Err(e) = vm::save(store, metadata) {
        eprintln!(
            "Warning: failed to update VM '{}' state: {e}",
            metadata.name
        );
    }
}

/// Best-effort network cleanup for a dead VM.
fn cleanup_network(store: &StateStore, vm_name: &str, net_info: &vm::NetworkInfo) {
    network::cleanup(store, vm_name, net_info);
}
