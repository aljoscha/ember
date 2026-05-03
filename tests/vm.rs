//! Integration tests for VM lifecycle commands.
//!
//! Cross-platform tests use `TestEnv` to abstract platform setup.
//! Tests that only need a stopped VM use `TestEnv::with_vm()`.
//! Tests that need a running VM use `TestEnv::with_running_vm()`.
//! Platform-specific networking tests are gated with `#[cfg(target_os)]`.
//!
//! To run:
//!   ./run-integration-tests.sh vm

#[allow(dead_code)]
mod common;

// ---------------------------------------------------------------------------
// Cross-platform tests (no hypervisor needed)
// ---------------------------------------------------------------------------

/// Create a VM with --no-start, verify vm list and vm inspect.
#[test]
#[ignore]
fn vm_create_and_inspect() {
    let env = common::TestEnv::with_vm("vmcreate", "testvm");
    let state = env.state();

    // Verify vm list shows the VM.
    let list_output = common::ember(&["--state-dir", state, "vm", "list"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(list_output.status.success());
    assert!(
        list_stdout.contains("testvm"),
        "expected 'testvm' in list: {list_stdout}"
    );
    assert!(
        list_stdout.contains("created"),
        "expected 'created' status in list: {list_stdout}"
    );

    // Verify vm inspect shows correct details.
    let inspect_output = common::ember(&["--state-dir", state, "vm", "inspect", "testvm"]);
    let inspect_stdout = String::from_utf8_lossy(&inspect_output.stdout);
    assert!(inspect_output.status.success());
    assert!(inspect_stdout.contains("testvm"));
    assert!(inspect_stdout.contains("created"));
    assert!(inspect_stdout.contains("alpine"));

    // Verify JSON inspect output.
    let json_output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "testvm",
        "--format",
        "json",
    ]);
    let json_stdout = String::from_utf8_lossy(&json_output.stdout);
    assert!(json_output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {json_stdout}"));
    assert_eq!(parsed["name"], "testvm");
    assert_eq!(parsed["status"], "created");
}

/// Creating a VM with a duplicate name should fail.
#[test]
#[ignore]
fn vm_create_duplicate_name_fails() {
    let env = common::TestEnv::with_vm("vmdup", "dupvm");
    let state = env.state();

    // Create a dummy kernel for the second create attempt.
    let kernel_tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(kernel_tmp.path(), b"not a real kernel").unwrap();

    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "dupvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel_tmp.path().to_str().unwrap(),
        "--no-start",
    ]);
    assert!(
        !output.status.success(),
        "expected duplicate create to fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' error: {stderr}"
    );
}

/// Delete a created VM, verify it's gone from list and storage.
#[test]
#[ignore]
fn vm_delete() {
    let env = common::TestEnv::with_vm("vmdel", "delvm");
    let state = env.state();

    // Platform-specific: verify storage exists before delete.
    #[cfg(target_os = "linux")]
    {
        let vm_zvol = format!("{}/ember/vms/delvm", env.pool);
        common::linux::assert_dataset_exists(&vm_zvol);
    }

    // Delete.
    let output = common::ember(&["--state-dir", state, "vm", "delete", "delvm"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm delete failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("deleted"),
        "expected 'deleted' in output: {stdout}"
    );

    // Verify vm list is empty.
    let list_output = common::ember(&["--state-dir", state, "vm", "list"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_stdout.contains("No VMs found"),
        "expected empty vm list: {list_stdout}"
    );

    // Platform-specific: verify storage is gone.
    #[cfg(target_os = "linux")]
    common::linux::assert_dataset_absent(&format!("{}/ember/vms/delvm", env.pool));

    #[cfg(target_os = "macos")]
    assert!(
        !env.state_dir.join("vms").join("delvm").exists(),
        "VM directory should not exist after delete"
    );
}

/// `ember vm list` shows table and JSON output correctly.
///
/// Creates two VMs, verifies both appear in table output and JSON array.
#[test]
#[ignore]
fn vm_list() {
    let env = common::TestEnv::with_vm("vmlist", "vm-alpha");
    let state = env.state();

    // Create a second VM with a dummy kernel.
    let kernel_tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(kernel_tmp.path(), b"not a real kernel").unwrap();
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "vm-beta",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel_tmp.path().to_str().unwrap(),
        "--no-start",
    ]);
    assert!(
        output.status.success(),
        "second vm create failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Table output: both VMs should appear.
    let list_output = common::ember(&["--state-dir", state, "vm", "list"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(list_output.status.success());
    assert!(
        list_stdout.contains("vm-alpha"),
        "expected 'vm-alpha' in list: {list_stdout}"
    );
    assert!(
        list_stdout.contains("vm-beta"),
        "expected 'vm-beta' in list: {list_stdout}"
    );

    // JSON output: should be an array with two entries.
    let json_output = common::ember(&["--state-dir", state, "vm", "list", "--format", "json"]);
    let json_stdout = String::from_utf8_lossy(&json_output.stdout);
    assert!(json_output.status.success());
    let parsed: serde_json::Value = serde_json::from_str(&json_stdout)
        .unwrap_or_else(|e| panic!("invalid JSON: {e}\noutput: {json_stdout}"));
    let arr = parsed.as_array().expect("expected JSON array from vm list");
    assert_eq!(arr.len(), 2, "expected 2 VMs in JSON list, got: {arr:?}");

    let names: Vec<&str> = arr.iter().map(|v| v["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"vm-alpha"), "missing vm-alpha in {names:?}");
    assert!(names.contains(&"vm-beta"), "missing vm-beta in {names:?}");
}

/// Stopping a created (not running) VM should fail with a state error.
#[test]
#[ignore]
fn vm_stop_created_fails() {
    let env = common::TestEnv::with_vm("vmstopstate", "stoptest");
    let state = env.state();

    let output = common::ember(&["--state-dir", state, "vm", "stop", "stoptest"]);
    assert!(
        !output.status.success(),
        "expected stop to fail for non-running VM"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("running or paused"),
        "expected state error: {stderr}"
    );
}

/// Deleting a nonexistent VM should fail.
#[test]
#[ignore]
fn vm_delete_nonexistent_fails() {
    let env = common::TestEnv::init("vmdelnoexist");
    let state = env.state();

    let output = common::ember(&["--state-dir", state, "vm", "delete", "nosuchvm"]);
    assert!(
        !output.status.success(),
        "expected delete of nonexistent VM to fail"
    );
}

/// Pausing a created (not running) VM should fail with a state error.
#[test]
#[ignore]
fn vm_pause_created_fails() {
    let env = common::TestEnv::with_vm("vmpausecreated", "pausetest");
    let state = env.state();

    let output = common::ember(&["--state-dir", state, "vm", "pause", "pausetest"]);
    assert!(
        !output.status.success(),
        "expected pause to fail for non-running VM"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("created") && stderr.contains("expected running"),
        "expected state error mentioning 'created' and 'expected running': {stderr}"
    );
}

/// Resuming a created (not paused) VM should fail with a state error.
#[test]
#[ignore]
fn vm_resume_created_fails() {
    let env = common::TestEnv::with_vm("vmresumecreated", "resumetest");
    let state = env.state();

    let output = common::ember(&["--state-dir", state, "vm", "resume", "resumetest"]);
    assert!(
        !output.status.success(),
        "expected resume to fail for non-paused VM"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("created") && stderr.contains("expected paused"),
        "expected state error mentioning 'created' and 'expected paused': {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Cross-platform tests (require running VM / hypervisor)
// ---------------------------------------------------------------------------

/// Full VM lifecycle: start → verify running → stop → verify stopped → delete.
///
/// Requires hypervisor prerequisites (Firecracker on Linux, ember-vz on macOS).
#[test]
#[ignore]
fn vm_start_stop_lifecycle() {
    let env = common::TestEnv::with_running_vm("vmlifecycle", "lifecyclevm");
    let state = env.state();

    // Verify running via inspect.
    let inspect = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "lifecyclevm",
        "--format",
        "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
        .expect("failed to parse inspect JSON");
    assert_eq!(
        json["status"], "running",
        "expected status 'running', got: {}",
        json["status"]
    );

    // Linux: verify Firecracker process is alive.
    #[cfg(target_os = "linux")]
    {
        let pid = json["pid"]
            .as_u64()
            .expect("expected numeric PID in inspect output");
        assert!(
            std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "expected Firecracker process (pid {pid}) to be alive"
        );
    }

    // Stop.
    let stop = common::ember(&["--state-dir", state, "vm", "stop", "lifecyclevm", "--force"]);
    let stop_stdout = String::from_utf8_lossy(&stop.stdout);
    let stop_stderr = String::from_utf8_lossy(&stop.stderr);
    assert!(
        stop.status.success(),
        "vm stop failed.\nstdout: {stop_stdout}\nstderr: {stop_stderr}"
    );
    assert!(
        stop_stdout.contains("stopped"),
        "expected 'stopped' in output: {stop_stdout}"
    );

    // Verify stopped.
    let inspect2 = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "lifecyclevm",
        "--format",
        "json",
    ]);
    assert!(inspect2.status.success());
    let json2: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect2.stdout))
        .expect("failed to parse inspect JSON after stop");
    assert_eq!(json2["status"], "stopped");
    assert!(json2["pid"].is_null(), "expected pid to be null after stop");

    // Delete.
    let del = common::ember(&["--state-dir", state, "vm", "delete", "lifecyclevm"]);
    let del_stdout = String::from_utf8_lossy(&del.stdout);
    let del_stderr = String::from_utf8_lossy(&del.stderr);
    assert!(
        del.status.success(),
        "vm delete failed.\nstdout: {del_stdout}\nstderr: {del_stderr}"
    );

    // Verify gone.
    let list = common::ember(&["--state-dir", state, "vm", "list"]);
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("No VMs found"),
        "expected empty vm list after delete: {list_stdout}"
    );
}

/// Delete a running VM requires --force.
#[test]
#[ignore]
fn vm_delete_running_requires_force() {
    let env = common::TestEnv::with_running_vm("vmdelrunning", "runningvm");
    let state = env.state();

    // Try delete without --force — should fail.
    let del = common::ember(&["--state-dir", state, "vm", "delete", "runningvm"]);
    assert!(
        !del.status.success(),
        "expected delete of running VM to fail without --force"
    );
    let stderr = String::from_utf8_lossy(&del.stderr);
    assert!(
        stderr.contains("--force"),
        "expected error mentioning --force: {stderr}"
    );

    // Delete with --force — should succeed.
    let force_del = common::ember(&["--state-dir", state, "vm", "delete", "runningvm", "--force"]);
    let force_stdout = String::from_utf8_lossy(&force_del.stdout);
    let force_stderr = String::from_utf8_lossy(&force_del.stderr);
    assert!(
        force_del.status.success(),
        "vm delete --force failed.\nstdout: {force_stdout}\nstderr: {force_stderr}"
    );

    // Platform-specific: verify storage cleanup.
    #[cfg(target_os = "linux")]
    common::linux::assert_dataset_absent(&format!("{}/ember/vms/runningvm", env.pool));
}

/// Pause/resume lifecycle: pause → verify → resume → verify → edge cases → cleanup.
///
/// Verifies:
/// - Pause transitions status from running to paused
/// - Resume transitions status from paused to running
/// - Pausing an already paused VM fails
/// - Resuming an already running VM fails
#[test]
#[ignore]
fn vm_pause_resume_lifecycle() {
    let env = common::TestEnv::with_running_vm("vmpauseresume", "prvm");
    let state = env.state();

    // Verify running.
    let inspect1 = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "prvm",
        "--format",
        "json",
    ]);
    assert!(inspect1.status.success());
    let json1: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect1.stdout))
        .expect("failed to parse inspect JSON");
    assert_eq!(json1["status"], "running");

    // Pause.
    let pause = common::ember(&["--state-dir", state, "vm", "pause", "prvm"]);
    let pause_stdout = String::from_utf8_lossy(&pause.stdout);
    let pause_stderr = String::from_utf8_lossy(&pause.stderr);
    assert!(
        pause.status.success(),
        "vm pause failed.\nstdout: {pause_stdout}\nstderr: {pause_stderr}"
    );
    assert!(
        pause_stdout.contains("paused"),
        "expected 'paused' in output: {pause_stdout}"
    );

    // Verify paused.
    let inspect2 = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "prvm",
        "--format",
        "json",
    ]);
    assert!(inspect2.status.success());
    let json2: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect2.stdout))
        .expect("failed to parse inspect JSON after pause");
    assert_eq!(
        json2["status"], "paused",
        "expected status 'paused', got: {}",
        json2["status"]
    );

    // Linux: verify process is still alive (paused, not killed).
    #[cfg(target_os = "linux")]
    {
        let pid = json2["pid"]
            .as_u64()
            .expect("expected numeric PID in inspect output");
        assert!(
            std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "Firecracker process (pid {pid}) should be alive while paused"
        );
    }

    // Pausing an already paused VM should fail.
    let pause_again = common::ember(&["--state-dir", state, "vm", "pause", "prvm"]);
    assert!(
        !pause_again.status.success(),
        "expected pause to fail for already-paused VM"
    );
    let pause_again_stderr = String::from_utf8_lossy(&pause_again.stderr);
    assert!(
        pause_again_stderr.contains("paused") && pause_again_stderr.contains("expected running"),
        "expected state error: {pause_again_stderr}"
    );

    // Resume.
    let resume = common::ember(&["--state-dir", state, "vm", "resume", "prvm"]);
    let resume_stdout = String::from_utf8_lossy(&resume.stdout);
    let resume_stderr = String::from_utf8_lossy(&resume.stderr);
    assert!(
        resume.status.success(),
        "vm resume failed.\nstdout: {resume_stdout}\nstderr: {resume_stderr}"
    );
    assert!(
        resume_stdout.contains("resumed"),
        "expected 'resumed' in output: {resume_stdout}"
    );

    // Verify running again.
    let inspect3 = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "prvm",
        "--format",
        "json",
    ]);
    assert!(inspect3.status.success());
    let json3: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect3.stdout))
        .expect("failed to parse inspect JSON after resume");
    assert_eq!(
        json3["status"], "running",
        "expected status 'running' after resume, got: {}",
        json3["status"]
    );

    // Resuming an already running VM should fail.
    let resume_again = common::ember(&["--state-dir", state, "vm", "resume", "prvm"]);
    assert!(
        !resume_again.status.success(),
        "expected resume to fail for already-running VM"
    );
    let resume_again_stderr = String::from_utf8_lossy(&resume_again.stderr);
    assert!(
        resume_again_stderr.contains("running") && resume_again_stderr.contains("expected paused"),
        "expected state error: {resume_again_stderr}"
    );

    // Stop and cleanup.
    let stop = common::ember(&["--state-dir", state, "vm", "stop", "prvm", "--force"]);
    assert!(
        stop.status.success(),
        "vm stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let del = common::ember(&["--state-dir", state, "vm", "delete", "prvm"]);
    assert!(
        del.status.success(),
        "vm delete failed: {}",
        String::from_utf8_lossy(&del.stderr)
    );
}

/// Stopping a paused VM should work (via --force).
#[test]
#[ignore]
fn vm_stop_paused() {
    let env = common::TestEnv::with_running_vm("vmstoppaused", "spvm");
    let state = env.state();

    // Pause.
    let pause = common::ember(&["--state-dir", state, "vm", "pause", "spvm"]);
    assert!(
        pause.status.success(),
        "vm pause failed: {}",
        String::from_utf8_lossy(&pause.stderr)
    );

    // Stop the paused VM with --force.
    let stop = common::ember(&["--state-dir", state, "vm", "stop", "spvm", "--force"]);
    let stop_stdout = String::from_utf8_lossy(&stop.stdout);
    let stop_stderr = String::from_utf8_lossy(&stop.stderr);
    assert!(
        stop.status.success(),
        "vm stop --force failed for paused VM.\nstdout: {stop_stdout}\nstderr: {stop_stderr}"
    );

    // Verify stopped.
    let inspect = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "spvm",
        "--format",
        "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
        .expect("failed to parse inspect JSON after stop");
    assert_eq!(json["status"], "stopped");

    // Cleanup.
    let del = common::ember(&["--state-dir", state, "vm", "delete", "spvm"]);
    assert!(
        del.status.success(),
        "vm delete failed: {}",
        String::from_utf8_lossy(&del.stderr)
    );
}

/// Force-stop a running VM: verify `vm stop --force` sends SIGKILL and
/// transitions status from running to stopped.
///
/// Requires hypervisor prerequisites.
#[test]
#[ignore]
fn vm_force_stop() {
    let env = common::TestEnv::with_running_vm("vmforcestop", "forcevm");
    let state = env.state();

    // Verify running.
    let inspect = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "forcevm",
        "--format",
        "json",
    ]);
    assert!(inspect.status.success());
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
        .expect("failed to parse inspect JSON");
    assert_eq!(json["status"], "running");

    // Force stop.
    let stop = common::ember(&["--state-dir", state, "vm", "stop", "forcevm", "--force"]);
    let stop_stdout = String::from_utf8_lossy(&stop.stdout);
    let stop_stderr = String::from_utf8_lossy(&stop.stderr);
    assert!(
        stop.status.success(),
        "vm stop --force failed.\nstdout: {stop_stdout}\nstderr: {stop_stderr}"
    );
    assert!(
        stop_stdout.contains("stopped") || stop_stdout.contains("Force-stopping"),
        "expected stop confirmation in output: {stop_stdout}"
    );

    // Verify stopped.
    let inspect2 = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "forcevm",
        "--format",
        "json",
    ]);
    assert!(inspect2.status.success());
    let json2: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect2.stdout))
        .expect("failed to parse inspect JSON after force stop");
    assert_eq!(
        json2["status"], "stopped",
        "expected stopped after force stop, got: {}",
        json2["status"]
    );
    assert!(
        json2["pid"].is_null(),
        "expected pid to be null after force stop"
    );

    // Cleanup.
    let del = common::ember(&["--state-dir", state, "vm", "delete", "forcevm"]);
    assert!(
        del.status.success(),
        "vm delete failed: {}",
        String::from_utf8_lossy(&del.stderr)
    );
}

// ---------------------------------------------------------------------------
// Linux-specific tests
// ---------------------------------------------------------------------------

/// Full networking test: start VM → verify TAP + iptables + ping → SSH → internet → stop.
///
/// Uses the `ubuntu-slim` image (built via Docker) which includes systemd,
/// openssh-server, and networking tools.
///
/// Requires: Firecracker, Docker, bootable kernel, SSH key pair, network access.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn networking_ssh_and_internet() {
    common::linux::require_firecracker();
    common::require_docker();
    let kernel_path = common::linux::ensure_kernel();

    let ssh_key = common::linux::ssh_private_key_path()
        .expect("no SSH private key found (~/.ssh/id_ed25519, id_ecdsa, or id_rsa)");

    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        common::linux::setup_pool_init_and_build_ubuntu("vmnetwork", &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // -- Create VM --
    let create_output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "netvm",
        "--image",
        "ubuntu-slim",
        "--cpus",
        "1",
        "--memory",
        "512M",
        "--kernel",
        kernel,
        "--no-start",
    ]);
    let stdout = String::from_utf8_lossy(&create_output.stdout);
    let stderr = String::from_utf8_lossy(&create_output.stderr);
    assert!(
        create_output.status.success(),
        "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // -- Start VM --
    let start_output = common::ember(&["--state-dir", state, "vm", "start", "netvm"]);
    let start_stdout = String::from_utf8_lossy(&start_output.stdout);
    let start_stderr = String::from_utf8_lossy(&start_output.stderr);
    assert!(
        start_output.status.success(),
        "vm start failed.\nstdout: {start_stdout}\nstderr: {start_stderr}"
    );

    // -- Inspect: verify network metadata --
    let inspect_output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "netvm",
        "--format",
        "json",
    ]);
    assert!(inspect_output.status.success());
    let inspect_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&inspect_output.stdout))
            .expect("failed to parse inspect JSON");

    assert_eq!(inspect_json["status"], "running");
    assert!(inspect_json["pid"].is_u64(), "expected numeric PID");

    let network = &inspect_json["network"];
    assert!(
        !network.is_null(),
        "expected network info in inspect output"
    );

    let tap_device = network["tap_device"]
        .as_str()
        .expect("expected tap_device string");
    let guest_ip = network["guest_ip"]
        .as_str()
        .expect("expected guest_ip string");
    let host_ip = network["host_ip"]
        .as_str()
        .expect("expected host_ip string");

    assert!(
        tap_device.starts_with("em") && tap_device.contains('-'),
        "TAP device should match em<id>-<vmid>, got: {tap_device}"
    );
    assert!(!guest_ip.is_empty(), "guest_ip should not be empty");
    assert!(!host_ip.is_empty(), "host_ip should not be empty");

    eprintln!("Network info: TAP={tap_device} host={host_ip} guest={guest_ip}");

    // -- Verify TAP device exists on host --
    let ip_link = std::process::Command::new("ip")
        .args(["link", "show", tap_device])
        .output()
        .expect("failed to run ip link show");
    assert!(
        ip_link.status.success(),
        "TAP device '{tap_device}' not found on host: {}",
        String::from_utf8_lossy(&ip_link.stderr)
    );

    // -- Verify iptables NAT rules --
    let iptables_nat = std::process::Command::new("iptables")
        .args(["-t", "nat", "-S", "POSTROUTING"])
        .output()
        .expect("failed to run iptables");
    let nat_rules = String::from_utf8_lossy(&iptables_nat.stdout);
    assert!(
        nat_rules.contains(guest_ip),
        "expected MASQUERADE rule for {guest_ip} in NAT table:\n{nat_rules}"
    );

    // -- Verify FORWARD chain rules --
    let iptables_fwd = std::process::Command::new("iptables")
        .args(["-S", "FORWARD"])
        .output()
        .expect("failed to run iptables");
    let fwd_rules = String::from_utf8_lossy(&iptables_fwd.stdout);
    assert!(
        fwd_rules.contains(tap_device),
        "expected FORWARD rules mentioning {tap_device}:\n{fwd_rules}"
    );

    // -- Ping guest from host --
    let mut ping_ok = false;
    for attempt in 1..=20 {
        let ping = std::process::Command::new("ping")
            .args(["-c", "1", "-W", "1", guest_ip])
            .output()
            .expect("failed to run ping");
        if ping.status.success() {
            eprintln!("Host-to-guest ping succeeded on attempt {attempt}");
            ping_ok = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert!(
        ping_ok,
        "failed to ping guest at {guest_ip} from host after 20 attempts"
    );

    // -- SSH into guest --
    eprintln!("Waiting for SSH to become available...");
    assert!(
        common::linux::wait_for_ssh(guest_ip, &ssh_key),
        "SSH not reachable at {guest_ip}:22 after timeout"
    );

    let hostname_result = common::linux::ssh_exec(guest_ip, &ssh_key, "hostname");
    assert!(
        hostname_result.is_ok(),
        "SSH command 'hostname' failed: {:?}",
        hostname_result.err()
    );
    let hostname = hostname_result.unwrap();
    eprintln!("Guest hostname: {hostname}");

    // -- Verify DNS resolution from guest --
    let resolv_result = common::linux::ssh_exec(guest_ip, &ssh_key, "cat /etc/resolv.conf");
    assert!(
        resolv_result.is_ok(),
        "failed to read /etc/resolv.conf: {:?}",
        resolv_result.err()
    );
    let resolv_contents = resolv_result.unwrap();
    eprintln!("Guest /etc/resolv.conf:\n{resolv_contents}");
    assert!(
        resolv_contents.contains("nameserver"),
        "expected nameserver entries in resolv.conf: {resolv_contents}"
    );

    let dns_result = common::linux::ssh_exec(guest_ip, &ssh_key, "ping -c 1 -W 5 example.com");
    assert!(
        dns_result.is_ok(),
        "DNS resolution failed — guest cannot resolve example.com: {:?}",
        dns_result.err()
    );
    eprintln!("Guest DNS resolution verified (ping example.com)");

    // -- Verify internet from guest --
    let inet_result = common::linux::ssh_exec(
        guest_ip,
        &ssh_key,
        "curl -sS -o /dev/null -w '%{http_code}' -m 15 http://example.com",
    );
    assert!(
        inet_result.is_ok(),
        "Guest internet access failed (curl http://example.com): {:?}",
        inet_result.err()
    );
    let http_code = inet_result.unwrap();
    assert!(
        http_code.starts_with('2') || http_code.starts_with('3'),
        "expected HTTP 2xx/3xx from example.com, got: {http_code}"
    );
    eprintln!("Guest internet access verified (curl http://example.com → {http_code})");

    // -- Stop VM --
    let stop_output = common::ember(&["--state-dir", state, "vm", "stop", "netvm", "--force"]);
    let stop_stdout = String::from_utf8_lossy(&stop_output.stdout);
    let stop_stderr = String::from_utf8_lossy(&stop_output.stderr);
    assert!(
        stop_output.status.success(),
        "vm stop failed.\nstdout: {stop_stdout}\nstderr: {stop_stderr}"
    );

    // -- Verify network cleanup after stop --
    let ip_link_after = std::process::Command::new("ip")
        .args(["link", "show", tap_device])
        .output()
        .expect("failed to run ip link show");
    assert!(
        !ip_link_after.status.success(),
        "TAP device '{tap_device}' should be gone after stop"
    );

    let iptables_nat_after = std::process::Command::new("iptables")
        .args(["-t", "nat", "-S", "POSTROUTING"])
        .output()
        .expect("failed to run iptables");
    let nat_rules_after = String::from_utf8_lossy(&iptables_nat_after.stdout);
    let guest_cidr = format!("{guest_ip}/32");
    assert!(
        !nat_rules_after.contains(&guest_cidr),
        "MASQUERADE rule for {guest_ip} should be gone after stop:\n{nat_rules_after}"
    );

    // -- Delete VM --
    let del_output = common::ember(&["--state-dir", state, "vm", "delete", "netvm"]);
    assert!(
        del_output.status.success(),
        "vm delete failed: {}",
        String::from_utf8_lossy(&del_output.stderr)
    );

    common::linux::assert_dataset_absent(&format!("{pool}/ember/vms/netvm"));
}

// ---------------------------------------------------------------------------
// macOS-specific tests
// ---------------------------------------------------------------------------

/// Verify that a VM booted with a static IP does not use DHCP.
///
/// Boots ember-vz directly with static IP boot args, verifies serial output
/// does not contain DHCP requests.
#[cfg(target_os = "macos")]
#[test]
#[ignore]
fn vm_boots_with_static_ip() {
    use std::time::Duration;

    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
    const STOP_TIMEOUT: Duration = Duration::from_secs(10);

    let ember_vz = common::macos::ember_vz_bin();
    eprintln!("Using ember-vz: {}", ember_vz.display());

    let kernel = common::macos::ensure_kernel();
    eprintln!("Using kernel: {}", kernel.display());

    let tmp = tempfile::tempdir().unwrap();
    let rootfs = common::macos::create_test_rootfs(tmp.path(), 64);
    let serial_log = tmp.path().join("console.log");

    // Use a static IP in the vmnet range.
    let guest_ip = "192.168.64.2";
    let boot_args = format!(
        "console=hvc0 root=/dev/vda rw ip={}::192.168.64.1:255.255.255.0:testvm:eth0:off",
        guest_ip
    );

    eprintln!("Starting VM with static IP {guest_ip}...");
    let (mut child, pid, read_file) =
        common::macos::spawn_ember_vz(&ember_vz, &kernel, &rootfs, &serial_log, &boot_args);

    let mac = match common::macos::read_mac_from_pipe(read_file, BOOT_TIMEOUT) {
        Some(m) => m,
        None => {
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            let _ = common::macos::wait_for_exit(&mut child, STOP_TIMEOUT);
            panic!("VM failed to boot (no MAC on ready-fd)");
        }
    };
    eprintln!("VM booted, MAC: {mac}");

    // Give the kernel a moment to configure the static IP.
    std::thread::sleep(Duration::from_secs(3));

    let serial = std::fs::read_to_string(&serial_log).unwrap_or_default();
    eprintln!("Serial log ({} bytes)", serial.len());

    // Stop VM.
    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    let _ = common::macos::wait_for_exit(&mut child, STOP_TIMEOUT);

    // The serial output should NOT contain DHCP requests since we used a static IP.
    assert!(
        !serial.contains("Sending DHCP requests"),
        "static IP boot should not trigger DHCP"
    );

    eprintln!("Network test passed: VM booted with static IP {guest_ip} (no DHCP)");
}
