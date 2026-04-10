//! Vsock CID allocation for VMs.
//!
//! Each VM with vsock enabled needs a unique guest CID (Context Identifier).
//! CIDs 0–2 are reserved (0 = hypervisor, 1 = reserved, 2 = host).
//! Allocations start at CID 3 and increment sequentially.
//!
//! Allocations are tracked in `vsock/cids.json` via the state store
//! with flock-based locking for concurrent safety.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::state::store::StateStore;

/// First allocatable guest CID (0–2 are reserved).
const MIN_CID: u32 = 3;

/// Maximum guest CID. The vsock CID space is 32 bits, but we cap at a
/// reasonable limit. Firecracker and AVF both use u32 CIDs.
const MAX_CID: u32 = 0xFFFF_FFFE; // 2^32 - 2 (0xFFFFFFFF is reserved)

/// Persisted CID allocation state, stored as `vsock/cids.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CidAllocations {
    /// Map from CID to VM name.
    pub allocations: HashMap<u32, String>,
}

/// Allocate a unique guest CID for a VM.
///
/// Finds the lowest available CID starting at 3, records the allocation,
/// and persists it to the state store. The entire read-modify-write cycle
/// is performed under an exclusive file lock, preventing concurrent
/// processes from allocating the same CID.
pub fn allocate(store: &StateStore, vm_name: &str) -> Result<u32> {
    let path = store.vsock_allocations_path();
    let vm = vm_name.to_string();

    store.update(&path, CidAllocations::default(), |allocs| {
        let cid = (MIN_CID..=MAX_CID)
            .find(|c| !allocs.allocations.contains_key(c))
            .ok_or_else(|| Error::Vsock("no free CIDs available".to_string()))?;

        allocs.allocations.insert(cid, vm);
        Ok(cid)
    })
}

/// Release a VM's CID allocation.
///
/// Removes all allocation entries for the given VM name, making the CID
/// available for reuse. Uses an exclusive lock for consistency.
/// Idempotent — does nothing if the VM has no allocation or the
/// allocations file doesn't exist.
pub fn release(store: &StateStore, vm_name: &str) -> Result<()> {
    let path = store.vsock_allocations_path();

    // If no allocations file exists yet, nothing to release.
    if !path.exists() {
        return Ok(());
    }

    store.update(&path, CidAllocations::default(), |allocs| {
        allocs.allocations.retain(|_, name| name != vm_name);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();
        (dir, store)
    }

    #[test]
    fn allocate_starts_at_3() {
        let (_dir, store) = test_store();
        let cid = allocate(&store, "vm1").unwrap();
        assert_eq!(cid, 3);
    }

    #[test]
    fn allocate_sequential() {
        let (_dir, store) = test_store();
        let c1 = allocate(&store, "vm1").unwrap();
        let c2 = allocate(&store, "vm2").unwrap();
        let c3 = allocate(&store, "vm3").unwrap();
        assert_eq!(c1, 3);
        assert_eq!(c2, 4);
        assert_eq!(c3, 5);
    }

    #[test]
    fn allocate_reuses_released_cid() {
        let (_dir, store) = test_store();
        allocate(&store, "vm1").unwrap();
        allocate(&store, "vm2").unwrap();
        allocate(&store, "vm3").unwrap();

        // Release the middle one (CID 4).
        release(&store, "vm2").unwrap();

        // Next allocation should reuse CID 4.
        let c4 = allocate(&store, "vm4").unwrap();
        assert_eq!(c4, 4);
    }

    #[test]
    fn release_idempotent() {
        let (_dir, store) = test_store();
        // Release with no allocations file at all.
        release(&store, "nonexistent").unwrap();

        // Allocate then release twice.
        allocate(&store, "vm1").unwrap();
        release(&store, "vm1").unwrap();
        release(&store, "vm1").unwrap();
    }

    #[test]
    fn release_only_removes_target_vm() {
        let (_dir, store) = test_store();
        allocate(&store, "vm1").unwrap();
        allocate(&store, "vm2").unwrap();
        allocate(&store, "vm3").unwrap();

        release(&store, "vm2").unwrap();

        // vm1 and vm3 should still be allocated.
        let path = store.vsock_allocations_path();
        let allocs: CidAllocations = store.read(&path).unwrap();
        assert_eq!(allocs.allocations.len(), 2);
        assert_eq!(allocs.allocations[&3], "vm1");
        assert_eq!(allocs.allocations[&5], "vm3");
    }

    #[test]
    fn allocations_persist_across_reads() {
        let (_dir, store) = test_store();
        allocate(&store, "vm1").unwrap();
        allocate(&store, "vm2").unwrap();

        let path = store.vsock_allocations_path();
        let allocs: CidAllocations = store.read(&path).unwrap();
        assert_eq!(allocs.allocations.len(), 2);
        assert_eq!(allocs.allocations[&3], "vm1");
        assert_eq!(allocs.allocations[&4], "vm2");
    }
}
