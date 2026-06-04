//! Lightweight state reconciliation for macOS.
//!
//! Called on every command invocation to fix stale state:
//!
//! For each VM in Running/Paused state, check if the ember-vz PID
//! is still alive. If dead, mark the VM as Stopped and release its
//! IP allocation.
//!
//! All operations are best-effort: errors are logged but never propagated,
//! since reconciliation should not block normal CLI operation.

use std::path::Path;

use crate::vm::MacosVm;
use ember_core::backend::VmBackend;
use ember_core::network;
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

    let vms = match vm::list(&store) {
        Ok(vms) => vms,
        Err(e) => {
            eprintln!("Warning: reconciliation failed to list VMs: {e}");
            return;
        }
    };

    for metadata in vms {
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
                mark_stopped(&store, &metadata);
                continue;
            }
        };

        if !MacosVm::is_running(pid) {
            // Process is dead — mark stopped and release IP.
            eprintln!(
                "Warning: VM '{}' process (pid {pid}) is dead, marking stopped",
                metadata.name
            );
            mark_stopped(&store, &metadata);
        }
    }
}

/// Mark a VM as Stopped, clearing its PID and network info,
/// and releasing its IP allocation.
fn mark_stopped(store: &StateStore, metadata: &vm::VmMetadata) {
    let _ = network::ip::release(store, &metadata.name);
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
