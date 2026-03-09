//! Lightweight state reconciliation for macOS.
//!
//! Called on every command invocation to fix stale state:
//!
//! 1. For each VM in Running/Paused state, check if the ember-vz PID
//!    is still alive. If dead, mark the VM as Stopped.
//!
//! 2. For running VMs with a "pending" guest IP, attempt discovery from
//!    vmnet DHCP leases or ARP table and persist the result.
//!
//! All operations are best-effort: errors are logged but never propagated,
//! since reconciliation should not block normal CLI operation.

use std::path::Path;

use crate::backend::macos::vm::MacosVm;
use crate::backend::{Network, NetworkBackend, VmBackend};
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

    let net_backend = Network::new(store.clone());

    for mut metadata in vms {
        match metadata.status {
            VmStatus::Running | VmStatus::Paused => {}
            _ => continue,
        }

        let pid = match metadata.pid {
            Some(pid) => pid,
            None => {
                eprintln!(
                    "Warning: VM '{}' is {} but has no PID, marking stopped",
                    metadata.name, metadata.status
                );
                mark_stopped(&store, &mut metadata);
                continue;
            }
        };

        if !MacosVm::is_running(pid) {
            // Process is dead — mark stopped.
            eprintln!(
                "Warning: VM '{}' process (pid {pid}) is dead, marking stopped",
                metadata.name
            );
            mark_stopped(&store, &mut metadata);
            continue;
        }

        // Process is alive — resolve pending guest IP if possible.
        if let Some(ref net) = metadata.network {
            if net.guest_ip == "pending" {
                if let Some(ref mac) = net.guest_mac {
                    if let Ok(ip) = net_backend.discover_guest_ip(mac) {
                        let mut net = net.clone();
                        net.guest_ip = ip;
                        metadata.network = Some(net);
                        if let Err(e) = vm::save(&store, &metadata) {
                            eprintln!(
                                "Warning: failed to save discovered IP for VM '{}': {e}",
                                metadata.name
                            );
                        }
                    }
                }
            }
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
