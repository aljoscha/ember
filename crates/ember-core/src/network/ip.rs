//! IP allocation from a configurable /N subnet in /30 blocks.
//!
//! Each VM gets a point-to-point /30 link: host gets .1, guest gets .2.
//! Allocations are tracked in `state.db` (SQLite, see [`crate::state::db`]).
//! The schema's `(subnet, block_index) PRIMARY KEY` plus `vm_name UNIQUE`
//! makes double-allocation structurally impossible — the second `INSERT`
//! for the same slot fails with a constraint violation.
//!
//! With the default /16 subnet (10.100.0.0/16), this supports ~16,384
//! concurrent VMs.
//!
//! Concurrency model: each call opens a fresh SQLite connection and runs
//! the allocation under `BEGIN IMMEDIATE`, which acquires the write lock
//! at transaction start (not lazily on first write). This eliminates the
//! SELECT/INSERT TOCTOU window that the prior JSON-with-flock store had,
//! which could hand the same /30 block to multiple parallel `vm start`
//! invocations after a crash recovery (SEC-459).

use std::collections::HashSet;
use std::net::Ipv4Addr;

use rusqlite::params;

use crate::error::{Error, Result};
use crate::state::db;
use crate::state::store::StateStore;

/// A single IP allocation for one VM.
#[derive(Debug, Clone, PartialEq)]
pub struct IpAllocation {
    /// Index of the /30 block within the base subnet.
    pub block_index: u32,
    /// Host-side IP — first usable address in the /30 (e.g., "10.100.0.1").
    pub host_ip: String,
    /// Guest-side IP — second usable address in the /30 (e.g., "10.100.0.2").
    pub guest_ip: String,
    /// Netmask for the /30 link ("255.255.255.252").
    pub netmask: String,
}

/// Default base subnet when none is configured.
pub const DEFAULT_SUBNET: &str = "10.100.0.0/16";

/// Netmask for a /30 subnet.
const NETMASK_30: &str = "255.255.255.252";

/// Parse a CIDR subnet string into (base address, prefix length).
fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u8)> {
    let (ip_str, prefix_str) = cidr
        .split_once('/')
        .ok_or_else(|| Error::Network(format!("invalid CIDR notation: {cidr}")))?;

    let ip: Ipv4Addr = ip_str
        .parse()
        .map_err(|e| Error::Network(format!("invalid IP in CIDR '{cidr}': {e}")))?;

    let prefix: u8 = prefix_str
        .parse()
        .map_err(|e| Error::Network(format!("invalid prefix in CIDR '{cidr}': {e}")))?;

    if prefix > 30 {
        return Err(Error::Network(format!(
            "subnet /{prefix} is too small for /30 allocations"
        )));
    }

    // Verify the IP is properly masked (no host bits set).
    let ip_u32 = u32::from(ip);
    let mask = if prefix == 0 {
        0u32
    } else {
        !((1u32 << (32 - prefix)) - 1)
    };
    if ip_u32 & mask != ip_u32 {
        return Err(Error::Network(format!(
            "IP {ip} has host bits set for /{prefix}"
        )));
    }

    Ok((ip, prefix))
}

/// Maximum number of /30 blocks that fit in a given prefix.
fn max_blocks(prefix_len: u8) -> u32 {
    // A /30 has 4 addresses. A /prefix has 2^(32-prefix) addresses.
    // max_blocks = 2^(32-prefix) / 4 = 2^(30-prefix)
    1u32 << (30 - prefix_len)
}

/// Compute the IP addresses for a given /30 block.
fn block_ips(base: Ipv4Addr, block_index: u32) -> IpAllocation {
    let base_u32 = u32::from(base);
    let network = base_u32 + block_index * 4;
    let host = Ipv4Addr::from(network + 1);
    let guest = Ipv4Addr::from(network + 2);

    IpAllocation {
        block_index,
        host_ip: host.to_string(),
        guest_ip: guest.to_string(),
        netmask: NETMASK_30.to_string(),
    }
}

/// Allocate a /30 block for a VM.
///
/// Finds the lowest-numbered available block in the subnet, records the
/// allocation in `state.db`, and returns the IP addresses for that block.
///
/// The full read-modify-write runs under a single `BEGIN IMMEDIATE`
/// transaction, so parallel allocators serialize at the database layer
/// rather than racing on a JSON file. The `(subnet, block_index)` primary
/// key plus the `vm_name UNIQUE` constraint make double-allocation
/// structurally impossible: even if the `find` logic regressed, a duplicate
/// `INSERT` would fail with `SQLITE_CONSTRAINT_PRIMARYKEY`.
pub fn allocate(store: &StateStore, subnet: &str, vm_name: &str) -> Result<IpAllocation> {
    let (base, prefix) = parse_cidr(subnet)?;
    let max = max_blocks(prefix);

    let mut conn = db::open(store.root())?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    // Verify the subnet hasn't changed since allocations started.
    let existing_subnet: Option<String> = tx
        .query_row("SELECT subnet FROM network_allocations LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .ok();
    if let Some(existing) = existing_subnet {
        if existing != subnet {
            return Err(Error::Network(format!(
                "subnet mismatch: state has '{existing}', requested '{subnet}'"
            )));
        }
    }

    let used: HashSet<u32> = tx
        .prepare("SELECT block_index FROM network_allocations WHERE subnet = ?1")?
        .query_map(params![subnet], |r| r.get::<_, u32>(0))?
        .collect::<std::result::Result<_, _>>()?;

    let block_index = (0..max).find(|i| !used.contains(i)).ok_or_else(|| {
        Error::Network(format!(
            "no free /30 blocks in {subnet} (all {max} blocks allocated)"
        ))
    })?;

    tx.execute(
        "INSERT INTO network_allocations (block_index, subnet, vm_name) VALUES (?1, ?2, ?3)",
        params![block_index, subnet, vm_name],
    )?;
    tx.commit()?;

    Ok(block_ips(base, block_index))
}

/// Release a VM's IP allocation.
///
/// Removes all allocation entries for the given VM name, making the /30
/// block available for reuse. Idempotent — does nothing if the VM has no
/// allocation.
pub fn release(store: &StateStore, vm_name: &str) -> Result<()> {
    let conn = db::open(store.root())?;
    conn.execute(
        "DELETE FROM network_allocations WHERE vm_name = ?1",
        params![vm_name],
    )?;
    Ok(())
}

/// Structured description of a single allocator-state divergence.
///
/// Replaces the prior `Vec<String>` representation (SEC-460): the old free-form
/// messages forced consumers to scrape VM names out of strings, which under-
/// flagged multi-VM cases and missed subnet-level anomalies entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anomaly {
    pub kind: AnomalyKind,
    /// Human-readable rendering, suitable for `eprintln!` to operators.
    pub message: String,
    /// Every VM implicated by this anomaly. Consumers (e.g. `ember vm list`)
    /// flag *all* of these, not just the first one — the prior split-the-
    /// message-on-whitespace heuristic only ever flagged the first match.
    /// Empty for subnet-level anomalies that don't map to a single VM set.
    pub vm_names: Vec<String>,
}

/// What kind of divergence the [`Anomaly`] describes. Carries enough
/// structured detail for callers to render their own messages, key into
/// other state, or surface in machine-readable form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnomalyKind {
    /// Two or more allocator rows share the same `(subnet, block_index)`.
    /// Should be impossible under the schema's PRIMARY KEY constraint.
    DuplicateSlot {
        subnet: String,
        block_index: u32,
        count: u32,
    },
    /// The same `vm_name` appears in more than one allocator row. Should
    /// be impossible under the schema's UNIQUE constraint on `vm_name`.
    DuplicateVmName { vm_name: String, count: u32 },
    /// Two VMs report the same `guest_ip` in their metadata. Detected by
    /// `ember vm list` (cross-references `vm_metadata.network.guest_ip`,
    /// which lives outside `network_allocations`), so this kind isn't
    /// produced by `check_invariants` itself — it's part of the public
    /// vocabulary so consumers and renderers can share one type.
    DuplicateGuestIp { ip: String },
}

/// Check that the allocator state is internally consistent.
///
/// Returns the list of detected anomalies (empty list = healthy). Used by
/// `ember vm list` to flag corrupted state — see SEC-459 for the failure
/// mode this catches (allocator-state-vs-running-VM divergence after a
/// crash that bypassed the proper allocator path).
///
/// Today's checks:
///   - No two VMs share a `(subnet, block_index)` — the schema already
///     enforces this; a violation here means the constraint was bypassed.
///   - No `vm_name` appears more than once across rows.
///
/// Both are belt-and-suspenders against a hypothetical schema drift; under
/// normal operation the SQL constraints catch them at insert time.
///
/// Returns `Err` if the underlying SQLite store can't be opened or queried.
/// Callers MUST surface this — a swallowed error in a corruption-detection
/// codepath is the worst possible failure mode (silent "all clear" while
/// the very thing we're checking is broken). See SEC-460 for the bug the
/// prior `unwrap_or_default()` callers exhibited.
pub fn check_invariants(store: &StateStore) -> Result<Vec<Anomaly>> {
    let conn = db::open(store.root())?;
    let mut anomalies = Vec::new();

    // Duplicate (subnet, block_index): aggregate vm_names per offending
    // slot via GROUP_CONCAT so we can attribute the anomaly to *every* VM
    // that landed in that slot, not just one (which the prior split-the-
    // message heuristic in `vm list` was forced to settle for).
    let dup_slots: Vec<(String, u32, i64, String)> = conn
        .prepare(
            "SELECT subnet, block_index, COUNT(*) AS n, GROUP_CONCAT(vm_name) AS names
             FROM network_allocations
             GROUP BY subnet, block_index
             HAVING n > 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (subnet, block_index, n, names) in dup_slots {
        let vm_names: Vec<String> = names
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        anomalies.push(Anomaly {
            kind: AnomalyKind::DuplicateSlot {
                subnet: subnet.clone(),
                block_index,
                count: n as u32,
            },
            message: format!(
                "duplicate slot: subnet={subnet} block_index={block_index} count={n} vms={}",
                vm_names.join(",")
            ),
            vm_names,
        });
    }

    let dup_names: Vec<(String, i64)> = conn
        .prepare(
            "SELECT vm_name, COUNT(*) AS n
             FROM network_allocations
             GROUP BY vm_name
             HAVING n > 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    for (vm_name, n) in dup_names {
        anomalies.push(Anomaly {
            kind: AnomalyKind::DuplicateVmName {
                vm_name: vm_name.clone(),
                count: n as u32,
            },
            message: format!("duplicate vm_name: {vm_name} count={n}"),
            vm_names: vec![vm_name],
        });
    }

    Ok(anomalies)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- CIDR parsing ---

    #[test]
    fn parse_cidr_valid() {
        let (ip, prefix) = parse_cidr("10.100.0.0/16").unwrap();
        assert_eq!(ip, Ipv4Addr::new(10, 100, 0, 0));
        assert_eq!(prefix, 16);
    }

    #[test]
    fn parse_cidr_slash_30() {
        let (ip, prefix) = parse_cidr("192.168.1.0/30").unwrap();
        assert_eq!(ip, Ipv4Addr::new(192, 168, 1, 0));
        assert_eq!(prefix, 30);
    }

    #[test]
    fn parse_cidr_rejects_slash_31() {
        assert!(parse_cidr("10.0.0.0/31").is_err());
    }

    #[test]
    fn parse_cidr_rejects_host_bits() {
        assert!(parse_cidr("10.100.0.1/16").is_err());
    }

    #[test]
    fn parse_cidr_rejects_no_slash() {
        assert!(parse_cidr("10.100.0.0").is_err());
    }

    // --- Block math ---

    #[test]
    fn max_blocks_slash_16() {
        assert_eq!(max_blocks(16), 16384);
    }

    #[test]
    fn max_blocks_slash_24() {
        assert_eq!(max_blocks(24), 64);
    }

    #[test]
    fn max_blocks_slash_30() {
        assert_eq!(max_blocks(30), 1);
    }

    #[test]
    fn block_ips_first() {
        let alloc = block_ips(Ipv4Addr::new(10, 100, 0, 0), 0);
        assert_eq!(alloc.host_ip, "10.100.0.1");
        assert_eq!(alloc.guest_ip, "10.100.0.2");
        assert_eq!(alloc.netmask, "255.255.255.252");
    }

    #[test]
    fn block_ips_second() {
        let alloc = block_ips(Ipv4Addr::new(10, 100, 0, 0), 1);
        assert_eq!(alloc.host_ip, "10.100.0.5");
        assert_eq!(alloc.guest_ip, "10.100.0.6");
    }

    #[test]
    fn block_ips_wraps_octet() {
        let alloc = block_ips(Ipv4Addr::new(10, 100, 0, 0), 64);
        assert_eq!(alloc.host_ip, "10.100.1.1");
        assert_eq!(alloc.guest_ip, "10.100.1.2");
    }

    // --- Allocate / release with state store ---

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();
        (dir, store)
    }

    #[test]
    fn allocate_first_block() {
        let (_dir, store) = test_store();
        let alloc = allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        assert_eq!(alloc.block_index, 0);
        assert_eq!(alloc.host_ip, "10.100.0.1");
        assert_eq!(alloc.guest_ip, "10.100.0.2");
    }

    #[test]
    fn allocate_sequential() {
        let (_dir, store) = test_store();
        let a1 = allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        let a2 = allocate(&store, "10.100.0.0/16", "vm2").unwrap();
        let a3 = allocate(&store, "10.100.0.0/16", "vm3").unwrap();

        assert_eq!(a1.block_index, 0);
        assert_eq!(a2.block_index, 1);
        assert_eq!(a3.block_index, 2);

        assert_eq!(a1.host_ip, "10.100.0.1");
        assert_eq!(a2.host_ip, "10.100.0.5");
        assert_eq!(a3.host_ip, "10.100.0.9");
    }

    #[test]
    fn allocate_reuses_released_block() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        allocate(&store, "10.100.0.0/16", "vm2").unwrap();
        allocate(&store, "10.100.0.0/16", "vm3").unwrap();

        // Release the middle one.
        release(&store, "vm2").unwrap();

        // Next allocation should reuse block 1.
        let a4 = allocate(&store, "10.100.0.0/16", "vm4").unwrap();
        assert_eq!(a4.block_index, 1);
        assert_eq!(a4.host_ip, "10.100.0.5");
    }

    #[test]
    fn allocate_exhausts_small_subnet() {
        let (_dir, store) = test_store();
        allocate(&store, "192.168.1.0/30", "vm1").unwrap();
        let err = allocate(&store, "192.168.1.0/30", "vm2").unwrap_err();
        assert!(err.to_string().contains("no free /30 blocks"));
    }

    #[test]
    fn allocate_rejects_subnet_mismatch() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        let err = allocate(&store, "10.200.0.0/16", "vm2").unwrap_err();
        assert!(err.to_string().contains("subnet mismatch"));
    }

    #[test]
    fn allocate_rejects_duplicate_vm_name() {
        // The schema's UNIQUE(vm_name) makes double-allocation impossible.
        // The allocator function shouldn't be called twice for the same VM,
        // but if it is, surface a clear error rather than silently succeeding.
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        let err = allocate(&store, "10.100.0.0/16", "vm1").unwrap_err();
        assert!(err.to_string().contains("UNIQUE") || err.to_string().contains("sqlite"));
    }

    #[test]
    fn release_idempotent() {
        let (_dir, store) = test_store();
        release(&store, "nonexistent").unwrap();

        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        release(&store, "vm1").unwrap();
        release(&store, "vm1").unwrap();
    }

    #[test]
    fn release_only_removes_target_vm() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        allocate(&store, "10.100.0.0/16", "vm2").unwrap();
        allocate(&store, "10.100.0.0/16", "vm3").unwrap();

        release(&store, "vm2").unwrap();

        let conn = db::open(store.root()).unwrap();
        let rows: Vec<(u32, String)> = conn
            .prepare("SELECT block_index, vm_name FROM network_allocations ORDER BY block_index")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(0, "vm1".to_string()), (2, "vm3".to_string())]);
    }

    // --- Concurrency stress test (SEC-459 regression) ---

    /// Spawn N OS threads, each calling `allocate()` against a shared store
    /// with a distinct VM name. A `Barrier` makes all threads enter
    /// `BEGIN IMMEDIATE` near-simultaneously — without it, the threads can
    /// serialize on scheduler luck and never actually exercise the race
    /// window. After all threads finish, assert:
    ///
    ///   - every call returned `Ok`
    ///   - every returned `block_index` is unique
    ///   - the database has exactly N rows
    ///
    /// This is the regression test for SEC-459. Before the SQLite migration,
    /// six parallel `ember vm start` invocations could each see the same
    /// "free" slot and each return the same block_index — silently — because
    /// the JSON store's flock was only held for the duration of a single
    /// read or write call, not the read-modify-write transaction.
    #[test]
    fn parallel_allocate_produces_unique_slots() {
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
                allocate(&store, "10.100.0.0/16", &format!("vm{i}")).unwrap()
            }));
        }

        let results: Vec<IpAllocation> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Every block_index is distinct.
        let unique_slots: HashSet<u32> = results.iter().map(|a| a.block_index).collect();
        assert_eq!(
            unique_slots.len(),
            N,
            "parallel allocate produced duplicate slots: {results:?}"
        );

        // DB has exactly N rows.
        let conn = db::open(store.root()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM network_allocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, N as i64);
    }

    // --- Invariant checker ---

    #[test]
    fn check_invariants_clean_store() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        allocate(&store, "10.100.0.0/16", "vm2").unwrap();
        let anomalies = check_invariants(&store).unwrap();
        assert!(anomalies.is_empty(), "unexpected anomalies: {anomalies:?}");
    }

    /// Drop the PRIMARY KEY / UNIQUE constraints on `network_allocations`
    /// so a test can inject otherwise-impossible duplicate rows. The
    /// schema's PK + UNIQUE makes the duplicates we want to test for
    /// structurally impossible — but `check_invariants` is the safety net
    /// for corrupted state that bypassed those constraints (manual edit,
    /// migration bug, etc.), so the test has to fake that scenario.
    fn drop_alloc_constraints(store: &StateStore) {
        let conn = db::open(store.root()).unwrap();
        conn.execute_batch(
            "
            BEGIN;
            CREATE TABLE network_allocations_tmp (
                block_index INTEGER NOT NULL,
                subnet      TEXT    NOT NULL,
                vm_name     TEXT    NOT NULL
            ) STRICT;
            INSERT INTO network_allocations_tmp
                SELECT * FROM network_allocations;
            DROP TABLE network_allocations;
            ALTER TABLE network_allocations_tmp RENAME TO network_allocations;
            COMMIT;
            ",
        )
        .unwrap();
    }

    fn force_duplicate_slot(store: &StateStore, subnet: &str, block_index: u32, vm_name: &str) {
        drop_alloc_constraints(store);
        let conn = db::open(store.root()).unwrap();
        conn.execute(
            "INSERT INTO network_allocations (block_index, subnet, vm_name)
             VALUES (?1, ?2, ?3)",
            params![block_index, subnet, vm_name],
        )
        .unwrap();
    }

    #[test]
    fn check_invariants_duplicate_slot_attributes_all_vms() {
        let (_dir, store) = test_store();
        // Allocate vm1 at slot 0 normally — this also pre-creates the
        // network_allocations table via the migration path.
        let alloc1 = allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        // Now bypass the PK and inject vm2 at the same slot.
        force_duplicate_slot(&store, "10.100.0.0/16", alloc1.block_index, "vm2");

        let anomalies = check_invariants(&store).unwrap();
        let dup_slot: Vec<&Anomaly> = anomalies
            .iter()
            .filter(|a| matches!(a.kind, AnomalyKind::DuplicateSlot { .. }))
            .collect();
        assert_eq!(
            dup_slot.len(),
            1,
            "expected exactly one DuplicateSlot anomaly, got: {anomalies:?}"
        );
        let a = dup_slot[0];
        // BOTH vms must be flagged — the prior `Vec<String>` representation
        // forced callers to scrape one name out of the message and miss the
        // others. SEC-460 is about closing exactly that gap.
        assert!(
            a.vm_names.contains(&"vm1".to_string()),
            "vm1 missing: {:?}",
            a.vm_names
        );
        assert!(
            a.vm_names.contains(&"vm2".to_string()),
            "vm2 missing: {:?}",
            a.vm_names
        );
        assert_eq!(a.vm_names.len(), 2);
        match &a.kind {
            AnomalyKind::DuplicateSlot {
                subnet,
                block_index,
                count,
            } => {
                assert_eq!(subnet, "10.100.0.0/16");
                assert_eq!(*block_index, alloc1.block_index);
                assert_eq!(*count, 2);
            }
            other => panic!("expected DuplicateSlot, got {other:?}"),
        }
    }

    #[test]
    fn check_invariants_duplicate_vm_name_lists_one_vm() {
        let (_dir, store) = test_store();
        allocate(&store, "10.100.0.0/16", "vm1").unwrap();
        // Insert a second row for the same vm_name at a different slot.
        // The UNIQUE(vm_name) constraint makes this impossible via the
        // public API; we bypass it the same way as the slot test.
        drop_alloc_constraints(&store);
        let conn = db::open(store.root()).unwrap();
        conn.execute(
            "INSERT INTO network_allocations (block_index, subnet, vm_name)
             VALUES (1, '10.100.0.0/16', 'vm1')",
            [],
        )
        .unwrap();

        let anomalies = check_invariants(&store).unwrap();
        let dup_name: Vec<&Anomaly> = anomalies
            .iter()
            .filter(|a| matches!(a.kind, AnomalyKind::DuplicateVmName { .. }))
            .collect();
        assert_eq!(
            dup_name.len(),
            1,
            "expected exactly one DuplicateVmName anomaly, got: {anomalies:?}"
        );
        let a = dup_name[0];
        assert_eq!(a.vm_names, vec!["vm1".to_string()]);
        match &a.kind {
            AnomalyKind::DuplicateVmName { vm_name, count } => {
                assert_eq!(vm_name, "vm1");
                assert_eq!(*count, 2);
            }
            other => panic!("expected DuplicateVmName, got {other:?}"),
        }
    }

    #[test]
    fn check_invariants_propagates_db_open_failure() {
        // SEC-460: don't let DB-open errors silently report "no anomalies".
        // Point the store at a path that can't be opened (a regular file
        // exists where the DB directory should be) — db::open should fail.
        let dir = tempfile::tempdir().unwrap();
        let blocking_file = dir.path().join("state");
        std::fs::write(&blocking_file, b"not a directory").unwrap();
        // StateStore::new(dir.path()/"state") would expect a directory; we
        // wrote a file there, which the underlying open path will error on.
        let store = StateStore::new(blocking_file);
        // init may or may not error depending on platform; we just want
        // check_invariants itself to NOT silently return Ok([]).
        let result = check_invariants(&store);
        assert!(
            result.is_err(),
            "expected check_invariants to surface DB open failure, got: {result:?}"
        );
    }
}
