//! Integration tests for `ember storage usage` and the best-effort
//! usage columns on `vm list`.
//!
//! Every test here is `#[ignore]`d, matching the rest of `tests/`:
//! `TestEnv` builds a real backend (a loopback ZFS pool on Linux, an
//! APFS temp dir on macOS) and needs root on Linux. `run-integration-tests.sh`
//! passes `--ignored`, so these run there and stay out of `cargo test`.
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

/// A freshly created VM occupies real space, and never reports holding
/// more than it references.
#[test]
#[ignore = "requires root + a real storage backend"]
fn usage_reports_vm_occupancy() {
    let env = TestEnv::with_vm("usage_vm", "usage-vm");
    let usage = usage_json(env.state());

    let vm = &usage["vms"]["usage-vm"];
    assert!(!vm.is_null(), "VM missing from usage report: {usage:#}");

    let exclusive = as_u64(&vm["exclusive"]);
    assert!(exclusive > 0, "a created VM occupies blocks: {vm:#}");
    assert!(as_u64(&vm["provisioned"]) > 0, "no virtual size: {vm:#}");

    // `referenced` is optional: dm-thin and ZFS report it, APFS does not.
    if let Some(referenced) = vm["referenced"].as_u64() {
        assert!(
            exclusive <= referenced,
            "exclusive ({exclusive}) must stay within referenced ({referenced})"
        );
    }

    // The pool has to account for at least what this VM occupies.
    let allocated = as_u64(&usage["pool"]["allocated"]);
    assert!(
        allocated >= exclusive,
        "pool allocated ({allocated}) is smaller than one VM's exclusive ({exclusive})"
    );
}

/// Images are reported alongside VMs, and hold no more than they
/// reference despite carrying a refreservation on ZFS.
#[test]
#[ignore = "requires root + a real storage backend"]
fn usage_reports_images() {
    let env = TestEnv::with_vm("usage_images", "usage-img-vm");
    let usage = usage_json(env.state());

    let images = usage["images"]
        .as_object()
        .unwrap_or_else(|| panic!("images is not an object: {usage:#}"));
    assert_eq!(images.len(), 1, "expected the one pulled image: {usage:#}");

    let image = images.values().next().unwrap();
    assert!(as_u64(&image["provisioned"]) > 0);
    if let Some(referenced) = image["referenced"].as_u64() {
        assert!(
            as_u64(&image["exclusive"]) <= referenced,
            "image exclusive exceeds referenced: {image:#}"
        );
    }
}

/// A fork shares blocks with its origin, which is the whole point of
/// the CoW backends. Every backend measures this, so nothing skips
/// here. It used to: APFS reported `referenced` as null and this test
/// returned early on macOS, which left the one assertion aimed at
/// sharing unrun on the backend whose sharing was broken.
#[test]
#[ignore = "requires root + a real storage backend"]
fn fork_shares_blocks_with_origin() {
    let env = TestEnv::with_vm("usage_fork", "usage-src");

    // `--no-start` because `TestEnv::with_vm` installs a dummy kernel
    // that cannot boot, and fork starts the copy by default.
    let output = ember(&[
        "--state-dir",
        env.state(),
        "vm",
        "fork",
        "usage-src",
        "usage-fork",
        "--no-start",
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

/// The pool figure counts a shared block once, no matter how many
/// volumes map it.
///
/// This is the regression test for the APFS backend reporting
/// `st_blocks` as occupancy. `st_blocks` counts the blocks a file maps
/// rather than the ones it owns, so summing it charged every shared
/// block once per clone and the pool figure grew each time a free clone
/// was made. With one image, two VMs and three forks it overstated real
/// occupancy by 2.6x.
///
/// macOS-only, deliberately. The same property should hold on ZFS and
/// dm-thin, but `PoolUsage::allocated` on ZFS also carries
/// refreservation and snapshot charges, so the honest form of this
/// assertion there is a different one. Writing it here without a pool
/// to run it against is how the bug it guards got in.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires root + a real storage backend"]
fn pool_counts_shared_blocks_once() {
    let env = TestEnv::with_vm("usage_shared", "usage-shared");

    let output = ember(&[
        "--state-dir",
        env.state(),
        "vm",
        "fork",
        "usage-shared",
        "usage-shared-fork",
        "--no-start",
    ]);
    assert!(
        output.status.success(),
        "fork failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let usage = usage_json(env.state());
    let volumes: Vec<&serde_json::Value> = usage["vms"]
        .as_object()
        .into_iter()
        .chain(usage["images"].as_object())
        .flat_map(|m| m.values())
        .collect();

    let referenced: u64 = volumes.iter().map(|v| as_u64(&v["referenced"])).sum();
    let allocated = as_u64(&usage["pool"]["allocated"]);

    // The fork shares nearly all of its origin, so counting each block
    // once has to come out strictly below counting them per volume.
    assert!(
        allocated < referenced,
        "pool allocated ({allocated}) must count shared blocks once, \
         but it is not below the sum of referenced ({referenced}): {usage:#}"
    );
}

/// `vm list` gains a USED column and keeps working regardless.
#[test]
#[ignore = "requires root + a real storage backend"]
fn vm_list_shows_used_column() {
    let env = TestEnv::with_vm("usage_list", "usage-list-vm");

    let output = ember(&["--state-dir", env.state(), "vm", "list"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("USED"), "no USED column:\n{stdout}");
    assert!(stdout.contains("usage-list-vm"), "VM missing:\n{stdout}");
}

/// Listing VMs must survive a backend that cannot answer, because one
/// common reason to list them is that storage is broken.
///
/// Linux-only: the break is a `config.json` pointing at a pool that
/// does not exist, and `MacosStorage` reads neither `pool` nor
/// `storage_path` (it derives every path from `state_dir`), so the same
/// edit leaves the APFS backend perfectly able to measure.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires root + a real storage backend"]
fn vm_list_survives_unmeasurable_storage() {
    let env = TestEnv::with_vm("usage_broken", "usage-broken-vm");

    let config_path = env.state_dir.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config["pool"] = serde_json::Value::String("ember-no-such-pool".to_string());
    config["storage_path"] = serde_json::Value::String("/nonexistent/ember-usage".to_string());
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

    let output = ember(&["--state-dir", env.state(), "vm", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "vm list must not fail when usage is unavailable: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // USED is the last column, so the VM's row ends in the dash.
    let row = stdout
        .lines()
        .find(|l| l.starts_with("usage-broken-vm"))
        .unwrap_or_else(|| panic!("VM missing from listing:\n{stdout}"));
    assert!(
        row.ends_with(" -"),
        "expected an unmeasurable USED column, got: {row:?}"
    );
}

// ---------------------------------------------------------------------------
// dm-thin
// ---------------------------------------------------------------------------

/// The dm-thin per-volume path: thin ids on the records have to join
/// against `thin_ls` rows read through a metadata snapshot, and that
/// snapshot has to be released again. A leaked one would make the
/// second call fail with EBUSY and would pin metadata blocks.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires root + dm-thin kernel module"]
fn dm_thin_usage_measures_volumes_and_releases_snapshot() {
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
        "2G",
        "--instance-id",
        "beef",
    ]);
    assert!(
        output.status.success(),
        "dm-thin init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // An empty pool still reports capacity and metadata.
    let usage = usage_json(&state);
    assert!(as_u64(&usage["pool"]["capacity"]) > 0);
    assert!(as_u64(&usage["pool"]["metadata"]["capacity"]) > 0);
    assert!(
        usage["pool"]["logical"].is_null(),
        "dm-thin does not compress"
    );

    // Now give it something to measure. Without a VM the maps stay
    // empty and the thin-id join is never exercised.
    common::require_docker();
    let output = ember(&["--state-dir", &state, "image", "pull", "alpine:latest"]);
    assert!(
        output.status.success(),
        "image pull failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let kernel = tmp.path().join("vmlinux-dummy");
    std::fs::write(&kernel, b"not a real kernel").unwrap();
    let output = ember(&[
        "--state-dir",
        &state,
        "vm",
        "create",
        "thinvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--no-start",
    ]);
    assert!(
        output.status.success(),
        "vm create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let usage = usage_json(&state);
    let vm = &usage["vms"]["thinvm"];
    assert!(!vm.is_null(), "thin id join produced nothing: {usage:#}");
    let exclusive = as_u64(&vm["exclusive"]);
    let referenced = as_u64(&vm["referenced"]);
    assert!(
        exclusive <= referenced,
        "exclusive ({exclusive}) exceeds referenced ({referenced})"
    );
    // A fresh clone shares nearly everything with the image base.
    assert!(referenced > 0, "clone references no blocks: {vm:#}");
    assert!(
        !usage["images"].as_object().unwrap().is_empty(),
        "image missing from usage report: {usage:#}"
    );

    // A third call can only succeed if the previous two released their
    // metadata snapshots.
    let _ = usage_json(&state);
}
