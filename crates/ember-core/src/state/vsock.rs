//! Vsock CID allocation for VMs.
//!
//! Each VM with vsock enabled needs a unique guest CID (Context Identifier).
//! CIDs 0–2 are reserved (0 = hypervisor, 1 = reserved, 2 = host).
//! Allocations start at CID 3 and increment sequentially.
//!
//! Allocations are tracked in `state.db` (SQLite, see [`crate::state::db`]).
//! The schema's `cid PRIMARY KEY` plus `vm_name UNIQUE` constraint makes
//! double-allocation structurally impossible — a duplicate `INSERT` for the
//! same CID fails with a constraint violation, so even if the read-modify-write
//! logic regressed, the schema would catch it.
//!
//! Concurrency model: each call opens a fresh SQLite connection and runs the
//! allocation under `BEGIN IMMEDIATE`, which acquires the write lock at
//! transaction start (not lazily on first write). This eliminates the
//! SELECT/INSERT TOCTOU window that the prior JSON-with-flock store had,
//! which could hand the same CID to multiple parallel `vm start` invocations
//! after a crash recovery (SEC-458, sibling of SEC-459 for the IP allocator).

use rusqlite::params;

use crate::error::{Error, Result};
use crate::state::db;
use crate::state::store::StateStore;

/// First allocatable guest CID (0–2 are reserved).
const MIN_CID: u32 = 3;

/// Maximum guest CID. The vsock CID space is 32 bits, but we cap at a
/// reasonable limit. Firecracker and AVF both use u32 CIDs.
const MAX_CID: u32 = 0xFFFF_FFFE; // 2^32 - 2 (0xFFFFFFFF is reserved)

/// Allocate a unique guest CID for a VM.
///
/// Finds the lowest available CID starting at 3, records the allocation in
/// `state.db`, and returns the CID.
///
/// The full read-modify-write runs under a single `BEGIN IMMEDIATE`
/// transaction, so parallel allocators serialize at the database layer
/// rather than racing on a JSON file. The `cid PRIMARY KEY` plus the
/// `vm_name UNIQUE` constraint make double-allocation structurally
/// impossible: even if the `find` logic regressed, a duplicate `INSERT`
/// would fail with `SQLITE_CONSTRAINT_PRIMARYKEY`.
pub fn allocate(store: &StateStore, vm_name: &str) -> Result<u32> {
    let mut conn = db::open(store.root())?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    // Pull existing CIDs sorted ascending; the lowest gap (or one past the
    // current max) is the next allocation. Iterating in SQL avoids loading
    // a HashSet for what is naturally a sequential scan.
    let mut stmt = tx.prepare("SELECT cid FROM vsock_allocations ORDER BY cid ASC")?;
    let mut rows = stmt.query([])?;

    let mut next = MIN_CID;
    while let Some(row) = rows.next()? {
        let used: u32 = row.get(0)?;
        if used > next {
            // Gap found.
            break;
        }
        if used == next {
            next = next
                .checked_add(1)
                .ok_or_else(|| Error::Vsock("no free CIDs available".to_string()))?;
        }
    }
    drop(rows);
    drop(stmt);

    if next > MAX_CID {
        return Err(Error::Vsock("no free CIDs available".to_string()));
    }

    tx.execute(
        "INSERT INTO vsock_allocations (cid, vm_name) VALUES (?1, ?2)",
        params![next, vm_name],
    )?;
    tx.commit()?;

    Ok(next)
}

/// Release a VM's CID allocation.
///
/// Removes all allocation entries for the given VM name, making the CID
/// available for reuse. Idempotent — does nothing if the VM has no
/// allocation.
pub fn release(store: &StateStore, vm_name: &str) -> Result<()> {
    let conn = db::open(store.root())?;
    conn.execute(
        "DELETE FROM vsock_allocations WHERE vm_name = ?1",
        params![vm_name],
    )?;
    Ok(())
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
    fn allocate_rejects_duplicate_vm_name() {
        // The schema's UNIQUE(vm_name) makes double-allocation impossible.
        // If the allocator is called twice for the same VM, surface a clear
        // error rather than silently succeeding.
        let (_dir, store) = test_store();
        allocate(&store, "vm1").unwrap();
        let err = allocate(&store, "vm1").unwrap_err();
        assert!(err.to_string().contains("UNIQUE") || err.to_string().contains("sqlite"));
    }

    #[test]
    fn release_idempotent() {
        let (_dir, store) = test_store();
        // Release with no allocations table populated.
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

        // vm1 (CID 3) and vm3 (CID 5) should still be allocated.
        let conn = db::open(store.root()).unwrap();
        let rows: Vec<(u32, String)> = conn
            .prepare("SELECT cid, vm_name FROM vsock_allocations ORDER BY cid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(3, "vm1".to_string()), (5, "vm3".to_string())]);
    }

    /// Spawn N OS threads, each calling `allocate()` against a shared store
    /// with a distinct VM name. A `Barrier` makes all threads enter
    /// `BEGIN IMMEDIATE` near-simultaneously — without it, the threads can
    /// serialize on scheduler luck and never actually exercise the race
    /// window. After all threads finish, assert:
    ///
    ///   - every call returned `Ok`
    ///   - every returned CID is unique
    ///   - the database has exactly N rows
    ///
    /// This is the regression test for SEC-458 (vsock-side counterpart of
    /// SEC-459's IP-allocator parallel test). Before the SQLite migration,
    /// six parallel `ember vm start` invocations could each see the same
    /// "free" CID and each return the same value — silently — because the
    /// JSON store's flock was only held for the duration of a single read or
    /// write call, not the read-modify-write transaction.
    #[test]
    fn parallel_allocate_produces_unique_cids() {
        use std::collections::HashSet;
        use std::sync::{Arc, Barrier};
        use std::thread;

        const N: usize = 50;

        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();
        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(N));

        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                // Wait until every thread has reached this point, then race
                // into `allocate()` together. This forces actual contention
                // on `BEGIN IMMEDIATE` instead of relying on scheduler luck.
                barrier.wait();
                allocate(&store, &format!("vm{i}")).unwrap()
            }));
        }

        let cids: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Every CID is distinct.
        let unique: HashSet<u32> = cids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            N,
            "parallel allocate produced duplicate CIDs: {cids:?}"
        );

        // DB has exactly N rows.
        let conn = db::open(store.root()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM vsock_allocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, N as i64);
    }
}
