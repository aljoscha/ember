//! Integration tests for `ember storage usage` and the best-effort
//! usage columns on `vm list`.
//!
//! The cross-platform tests run against whatever backend `TestEnv`
//! sets up (ZFS on Linux, APFS on macOS). The dm-thin variants are
//! gated behind `#[ignore]` like the rest of `tests/dm_thin.rs`, since
//! they need root and the `dm-thin-pool` kernel module:
//!
//! ```text
//! sudo cargo test --test storage_usage -- --ignored --test-threads=1
//! ```

#[allow(dead_code)]
mod common;

use common::{ember, TestEnv};

/// Parse the JSON output of `ember storage usage`.
fn usage_json(state_dir: &str) -> serde_json::Value {
    let output = ember(&[
        "--state-dir",
        state_dir,
        "storage",
        "usage",
        "--format",
        "json",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "storage usage failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON: {e}\n{stdout}"))
}

fn as_u64(value: &serde_json::Value) -> u64 {
    value
        .as_u64()
        .unwrap_or_else(|| panic!("not a u64: {value}"))
}

/// A freshly created VM occupies real space, and whatever it
/// references is at least what it exclusively owns.
#[test]
fn usage_reports_vm_occupancy() {
    let env = TestEnv::with_vm("usage_vm", "usage-vm");
    let usage = usage_json(env.state());

    let vm = &usage["vms"]["usage-vm"];
    assert!(!vm.is_null(), "VM missing from usage report: {usage:#}");

    let exclusive = as_u64(&vm["exclusive"]);
    let provisioned = as_u64(&vm["provisioned"]);
    assert!(provisioned > 0, "provisioned size should be known");

    // `referenced` is optional: dm-thin and ZFS report it, APFS does not.
    if let Some(referenced) = vm["referenced"].as_u64() {
        assert!(
            referenced >= exclusive,
            "referenced ({referenced}) must include exclusive ({exclusive})"
        );
    }

    // The pool has to account for at least what this VM occupies.
    let allocated = as_u64(&usage["pool"]["allocated"]);
    assert!(
        allocated >= exclusive,
        "pool allocated ({allocated}) is smaller than one VM's exclusive ({exclusive})"
    );
}

/// Images are reported alongside VMs.
#[test]
fn usage_reports_images() {
    let env = TestEnv::with_vm("usage_images", "usage-img-vm");
    let usage = usage_json(env.state());

    let images = usage["images"]
        .as_object()
        .unwrap_or_else(|| panic!("images is not an object: {usage:#}"));
    assert_eq!(images.len(), 1, "expected the one pulled image: {usage:#}");
    let image = images.values().next().unwrap();
    assert!(as_u64(&image["provisioned"]) > 0);
}

/// A fork shares blocks with its origin, which is the whole point of
/// the CoW backends. Backends that cannot measure sharing report
/// `referenced` as null and are skipped.
#[test]
fn fork_shares_blocks_with_origin() {
    let env = TestEnv::with_vm("usage_fork", "usage-src");

    let output = ember(&[
        "--state-dir",
        env.state(),
        "vm",
        "fork",
        "usage-src",
        "usage-fork",
    ]);
    assert!(
        output.status.success(),
        "fork failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let usage = usage_json(env.state());
    let fork = &usage["vms"]["usage-fork"];
    assert!(!fork.is_null(), "fork missing from usage report: {usage:#}");

    let Some(referenced) = fork["referenced"].as_u64() else {
        return; // Backend cannot separate shared from exclusive.
    };
    let exclusive = as_u64(&fork["exclusive"]);
    assert!(
        referenced > exclusive,
        "a fresh fork should share blocks with its origin, \
         but referenced ({referenced}) <= exclusive ({exclusive})"
    );
}

/// `vm list` gains a USED column and keeps working regardless.
#[test]
fn vm_list_shows_used_column() {
    let env = TestEnv::with_vm("usage_list", "usage-list-vm");

    let output = ember(&["--state-dir", env.state(), "vm", "list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("USED"), "no USED column:\n{stdout}");
    assert!(stdout.contains("usage-list-vm"), "VM missing:\n{stdout}");
}

/// Listing VMs must survive a backend that cannot answer, because one
/// common reason to list them is that storage is broken. We simulate
/// that by pointing the config at a pool that does not exist.
#[test]
fn vm_list_survives_unmeasurable_storage() {
    let env = TestEnv::with_vm("usage_broken", "usage-broken-vm");

    let config_path = env.state_dir.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    if let Some(pool) = config.get_mut("pool") {
        *pool = serde_json::Value::String("ember-no-such-pool".to_string());
    }
    if let Some(path) = config.get_mut("storage_path") {
        *path = serde_json::Value::String("/nonexistent/ember-usage-test".to_string());
    }
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    let output = ember(&["--state-dir", env.state(), "vm", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "vm list must not fail when usage is unavailable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("usage-broken-vm"), "VM missing:\n{stdout}");
    assert!(
        stdout.contains(" -"),
        "expected a dash for the unmeasurable USED column:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// dm-thin
// ---------------------------------------------------------------------------

/// The metadata snapshot taken to measure per-volume usage must be
/// released again, otherwise the next reader gets EBUSY and the pool
/// keeps pinning metadata blocks. Running the command twice is the
/// cheapest way to prove the guard fired.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires root + dm-thin kernel module"]
fn dm_thin_usage_releases_metadata_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let storage_path = tmp.path().join("dm-thin");
    let state_dir = tmp.path().join("state");
    let state = state_dir.to_str().unwrap().to_string();

    let _cleanup = common::linux::DmThinCleanup {
        state_dir: state_dir.clone(),
    };

    let output = ember(&[
        "--state-dir",
        &state,
        "init",
        "--storage",
        "dm-thin",
        "--storage-path",
        storage_path.to_str().unwrap(),
        "--size",
        "500M",
        "--instance-id",
        "beef",
    ]);
    assert!(
        output.status.success(),
        "dm-thin init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Twice: the second call can only succeed if the first released.
    for attempt in 1..=2 {
        let output = ember(&["--state-dir", &state, "storage", "usage"]);
        assert!(
            output.status.success(),
            "storage usage attempt {attempt} failed, \
             metadata snapshot likely leaked: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // The pool line is always present, even with no volumes yet.
    let output = ember(&["--state-dir", &state, "storage", "usage"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Pool"), "no pool line:\n{stdout}");
    assert!(stdout.contains("Metadata"), "no metadata line:\n{stdout}");
}
