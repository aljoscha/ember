//! Integration tests for `ember vm update-config`.
//!
//! Uses `TestEnv::with_vm()` to create a stopped VM, then verifies that
//! update-config modifies metadata correctly and rejects invalid usage.
//!
//! To run:
//!   ./run-integration-tests.sh update_config

#[allow(dead_code)]
mod common;

/// Helper: inspect a VM and return the parsed JSON metadata.
fn inspect_json(state: &str, vm_name: &str) -> serde_json::Value {
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        vm_name,
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
        .expect("failed to parse inspect JSON")
}

/// Update cpus and memory, verify metadata changes.
#[test]
#[ignore]
fn update_config_cpus_and_memory() {
    let env = common::TestEnv::with_vm("updcfg1", "ucvm1");
    let state = env.state();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "update-config",
        "ucvm1",
        "--cpus",
        "4",
        "--memory",
        "2G",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "update-config failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("Updated VM"),
        "expected confirmation: {stdout}"
    );

    let json = inspect_json(state, "ucvm1");
    assert_eq!(json["cpus"], 4, "cpus should be 4");
    assert_eq!(json["memory_mib"], 2048, "memory should be 2048 MiB");
}

/// Update boot args, then clear them with an empty string.
#[test]
#[ignore]
fn update_config_boot_args() {
    let env = common::TestEnv::with_vm("updcfg2", "ucvm2");
    let state = env.state();

    // Set boot args.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "update-config",
        "ucvm2",
        "--boot-args",
        "console=ttyS0 panic=1",
    ]);
    assert!(
        output.status.success(),
        "update-config failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = inspect_json(state, "ucvm2");
    assert_eq!(
        json["boot_args"], "console=ttyS0 panic=1",
        "boot_args should be set"
    );

    // Clear boot args with empty string.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "update-config",
        "ucvm2",
        "--boot-args",
        "",
    ]);
    assert!(
        output.status.success(),
        "update-config clear failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = inspect_json(state, "ucvm2");
    assert!(
        json["boot_args"].is_null(),
        "boot_args should be cleared (null)"
    );
}

/// Update SSH user and key.
#[test]
#[ignore]
fn update_config_ssh_settings() {
    let env = common::TestEnv::with_vm("updcfg3", "ucvm3");
    let state = env.state();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "update-config",
        "ucvm3",
        "--ssh-user",
        "ubuntu",
        "--ssh-key",
        "/tmp/test-key",
    ]);
    assert!(
        output.status.success(),
        "update-config failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json = inspect_json(state, "ucvm3");
    assert_eq!(json["ssh"]["user"], "ubuntu", "ssh user should be ubuntu");
    assert_eq!(
        json["ssh"]["key"], "/tmp/test-key",
        "ssh key should be /tmp/test-key"
    );
}

/// Running update-config with no flags should fail.
#[test]
#[ignore]
fn update_config_no_changes_fails() {
    let env = common::TestEnv::with_vm("updcfg4", "ucvm4");
    let state = env.state();

    let output = common::ember(&["--state-dir", state, "vm", "update-config", "ucvm4"]);
    assert!(
        !output.status.success(),
        "expected update-config with no flags to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no configuration changes specified"),
        "expected 'no configuration changes' error: {stderr}"
    );
}

/// Updating a nonexistent VM should fail.
#[test]
#[ignore]
fn update_config_nonexistent_vm_fails() {
    let env = common::TestEnv::init("updcfg5");
    let state = env.state();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "update-config",
        "nosuchvm",
        "--cpus",
        "2",
    ]);
    assert!(
        !output.status.success(),
        "expected update-config of nonexistent VM to fail"
    );
}

/// Setting cpus to 0 should fail.
#[test]
#[ignore]
fn update_config_zero_cpus_fails() {
    let env = common::TestEnv::with_vm("updcfg6", "ucvm6");
    let state = env.state();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "update-config",
        "ucvm6",
        "--cpus",
        "0",
    ]);
    assert!(
        !output.status.success(),
        "expected update-config with 0 cpus to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cpus must be at least 1"),
        "expected cpus validation error: {stderr}"
    );
}
