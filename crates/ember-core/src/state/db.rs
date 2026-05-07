//! SQLite-backed allocator state.
//!
//! Replaces the prior `network/allocations.json` file with a `state.db`
//! SQLite database. Schema constraints (PRIMARY KEY and UNIQUE) make
//! double-allocation structurally impossible — the second `INSERT` for the
//! same slot fails with a constraint violation, so the allocator code can't
//! accidentally hand out the same IP twice even if the read-modify-write
//! logic regresses. Both the IP allocator (see [`crate::network::ip`]) and
//! the vsock CID allocator (see [`crate::state::vsock`]) live in this same
//! database; future allocators should add their own table here.
//!
//! See SEC-459 for the original TOCTOU bug this replaces. The flock-based
//! JSON store had per-call (not per-transaction) locking; six parallel
//! `ember vm start` invocations could each see an empty allocations file,
//! pick the same slot, and configure their NIC with the same IP. The last
//! writer's persisted state lied about what each VM had actually been
//! configured with.
//!
//! Each call opens a fresh connection. SQLite serializes concurrent writers
//! via filesystem locking, and `BEGIN IMMEDIATE` (used by the allocator
//! callers) acquires the write lock at transaction start rather than lazily,
//! eliminating the SELECT/INSERT race window.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::Result;

/// Schema bootstrap. Idempotent — safe to run on every connection open.
///
/// `STRICT` tables enforce the declared column types at the SQLite layer;
/// without it, SQLite accepts any value for any column. Keeps the
/// "constraints catch corruption" property robust.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS network_allocations (
    block_index INTEGER NOT NULL,
    subnet      TEXT    NOT NULL,
    vm_name     TEXT    NOT NULL UNIQUE,
    PRIMARY KEY (subnet, block_index)
) STRICT;

CREATE TABLE IF NOT EXISTS vsock_allocations (
    cid     INTEGER NOT NULL PRIMARY KEY,
    vm_name TEXT    NOT NULL UNIQUE
) STRICT;
"#;

/// Path to the allocator state database within a state store root.
pub fn db_path(root: &Path) -> PathBuf {
    root.join("state.db")
}

/// Open (or create) the allocator state database, applying the schema.
///
/// The database file lives at `<root>/state.db`. Parent directories must
/// already exist (the store's `init()` ensures this).
///
/// WAL mode is enabled to allow concurrent readers alongside a single
/// writer. Combined with `BEGIN IMMEDIATE` at the call site, this prevents
/// the SELECT/INSERT TOCTOU window from the prior JSON store.
pub fn open(root: &Path) -> Result<Connection> {
    let path = db_path(root);
    let conn = Connection::open(&path)?;
    // Set busy_timeout BEFORE the WAL conversion: the first time a fresh
    // database is opened, `journal_mode=WAL` performs a one-time conversion
    // that briefly takes a write lock. If two processes race the first
    // open, one would otherwise see SQLITE_BUSY without retry. After the
    // first conversion, the pragma is a no-op.
    // 5s busy timeout is enough for normal contention on a laptop pool;
    // contended writes serialize behind a held write lock.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn open_creates_database_and_schema() {
        let dir = tmp_root();
        let conn = open(dir.path()).unwrap();
        // network_allocations table exists with expected columns.
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('network_allocations')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(cols.contains(&"block_index".to_string()));
        assert!(cols.contains(&"subnet".to_string()));
        assert!(cols.contains(&"vm_name".to_string()));
    }

    #[test]
    fn open_is_idempotent() {
        let dir = tmp_root();
        let _conn1 = open(dir.path()).unwrap();
        // Re-opening must not fail or duplicate the schema.
        let _conn2 = open(dir.path()).unwrap();
    }

    #[test]
    fn primary_key_rejects_duplicate_slot_in_same_subnet() {
        let dir = tmp_root();
        let conn = open(dir.path()).unwrap();
        conn.execute(
            "INSERT INTO network_allocations (block_index, subnet, vm_name) VALUES (?1, ?2, ?3)",
            rusqlite::params![0u32, "10.100.0.0/16", "vm1"],
        )
        .unwrap();
        // Same (subnet, block_index) with a different vm_name must fail.
        let err = conn
            .execute(
                "INSERT INTO network_allocations (block_index, subnet, vm_name) VALUES (?1, ?2, ?3)",
                rusqlite::params![0u32, "10.100.0.0/16", "vm2"],
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("UNIQUE") || msg.contains("PRIMARY KEY"),
            "got: {msg}"
        );
    }

    #[test]
    fn open_creates_vsock_allocations_table() {
        let dir = tmp_root();
        let conn = open(dir.path()).unwrap();
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('vsock_allocations')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(cols.contains(&"cid".to_string()));
        assert!(cols.contains(&"vm_name".to_string()));
    }

    #[test]
    fn vsock_primary_key_rejects_duplicate_cid() {
        let dir = tmp_root();
        let conn = open(dir.path()).unwrap();
        conn.execute(
            "INSERT INTO vsock_allocations (cid, vm_name) VALUES (?1, ?2)",
            rusqlite::params![3u32, "vm1"],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO vsock_allocations (cid, vm_name) VALUES (?1, ?2)",
                rusqlite::params![3u32, "vm2"],
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("UNIQUE") || msg.contains("PRIMARY KEY"),
            "got: {msg}"
        );
    }

    #[test]
    fn vsock_vm_name_unique_constraint_rejects_double_allocation() {
        let dir = tmp_root();
        let conn = open(dir.path()).unwrap();
        conn.execute(
            "INSERT INTO vsock_allocations (cid, vm_name) VALUES (?1, ?2)",
            rusqlite::params![3u32, "vm1"],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO vsock_allocations (cid, vm_name) VALUES (?1, ?2)",
                rusqlite::params![4u32, "vm1"],
            )
            .unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "got: {err}");
    }

    #[test]
    fn vm_name_unique_constraint_rejects_double_allocation() {
        let dir = tmp_root();
        let conn = open(dir.path()).unwrap();
        conn.execute(
            "INSERT INTO network_allocations (block_index, subnet, vm_name) VALUES (?1, ?2, ?3)",
            rusqlite::params![0u32, "10.100.0.0/16", "vm1"],
        )
        .unwrap();
        // Same vm_name with a different slot must also fail — one VM = one slot.
        let err = conn
            .execute(
                "INSERT INTO network_allocations (block_index, subnet, vm_name) VALUES (?1, ?2, ?3)",
                rusqlite::params![1u32, "10.100.0.0/16", "vm1"],
            )
            .unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "got: {err}");
    }
}
