//! Integration tests for `ember vm create`, `vm start`, `vm stop`, and `vm delete` (Linux-only).
//!
//! These tests require:
#![cfg(target_os = "linux")]
//!
//! - Root privileges
//! - Working ZFS installation
//! - Network access (to pull OCI images from Docker Hub)
//! - `skopeo` installed
//!
//! Tests that start a VM additionally require:
//! - `firecracker` binary installed and in PATH
//! - A Linux kernel image (auto-downloaded to `/tmp/ember-test-vmlinux` on
//!   first run; override with `EMBER_TEST_KERNEL` env var)
//!
//! They are marked `#[ignore]` so `cargo test` skips them by default.
//!
//! To run:
//!   ./run-integration-tests.sh vm

#[allow(dead_code)]
mod common;

use std::path::Path;

// ---------------------------------------------------------------------------
// Tests that only need ZFS (no Firecracker required)
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn create_vm_shows_in_list_and_inspect() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = common::linux::setup_pool_init_and_pull("vmcreate", &tmp);
    let kernel = common::linux::create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create a VM (--no-start so we don't need Firecracker).
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "testvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--no-start",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vm create failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("created successfully"),
        "expected success message: {stdout}"
    );

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

#[test]
#[ignore]
fn create_duplicate_vm_name_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = common::linux::setup_pool_init_and_pull("vmdup", &tmp);
    let kernel = common::linux::create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create first VM.
    let output1 = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "dupvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--no-start",
    ]);
    assert!(
        output1.status.success(),
        "first create failed: {}",
        String::from_utf8_lossy(&output1.stderr)
    );

    // Try creating with the same name — should fail.
    let output2 = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "dupvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--no-start",
    ]);
    assert!(
        !output2.status.success(),
        "expected duplicate create to fail"
    );
    let stderr = String::from_utf8_lossy(&output2.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' error: {stderr}"
    );
}

#[test]
#[ignore]
fn delete_created_vm_cleans_up_zvol_and_state() {
    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_init_and_pull("vmdel", &tmp);
    let kernel = common::linux::create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create a VM.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "delvm",
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

    let vm_zvol = format!("{pool}/ember/vms/delvm");
    common::linux::assert_dataset_exists(&vm_zvol);

    // Delete it.
    let del_output = common::ember(&["--state-dir", state, "vm", "delete", "delvm"]);
    let del_stdout = String::from_utf8_lossy(&del_output.stdout);
    let del_stderr = String::from_utf8_lossy(&del_output.stderr);
    assert!(
        del_output.status.success(),
        "vm delete failed.\nstdout: {del_stdout}\nstderr: {del_stderr}"
    );
    assert!(
        del_stdout.contains("deleted"),
        "expected 'deleted' in output: {del_stdout}"
    );

    // Verify zvol is gone.
    common::linux::assert_dataset_absent(&vm_zvol);

    // Verify vm list is empty.
    let list_output = common::ember(&["--state-dir", state, "vm", "list"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_stdout.contains("No VMs found"),
        "expected empty vm list: {list_stdout}"
    );
}

#[test]
#[ignore]
fn stop_created_vm_fails_with_wrong_state() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) = common::linux::setup_pool_init_and_pull("vmstopstate", &tmp);
    let kernel = common::linux::create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create a VM but don't start it.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "stoptest",
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

    // Try to stop a created (not running) VM — should fail.
    let stop_output = common::ember(&["--state-dir", state, "vm", "stop", "stoptest"]);
    assert!(
        !stop_output.status.success(),
        "expected stop to fail for non-running VM"
    );
    let stderr = String::from_utf8_lossy(&stop_output.stderr);
    assert!(
        stderr.contains("running or paused"),
        "expected state error: {stderr}"
    );
}

#[test]
#[ignore]
fn delete_nonexistent_vm_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_init_and_pull("vmdelnoexist", &tmp);
    let state = state_dir.to_str().unwrap();

    let output = common::ember(&["--state-dir", state, "vm", "delete", "nosuchvm"]);
    assert!(
        !output.status.success(),
        "expected delete of nonexistent VM to fail"
    );
}

// ---------------------------------------------------------------------------
// Full lifecycle test (requires Firecracker + kernel)
// ---------------------------------------------------------------------------

/// Full VM lifecycle: create → start → verify running → stop → delete.
///
/// Requires `firecracker` in PATH and a bootable kernel (auto-downloaded
/// or overridden via `EMBER_TEST_KERNEL`). Skips if prerequisites are missing.
#[test]
#[ignore]
fn full_vm_lifecycle_start_stop_delete() {
    if !common::linux::firecracker_available() {
        return;
    }

    let kernel_path = match common::linux::ensure_kernel() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_init_and_pull("vmlifecycle", &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // -- Create --
    let create_output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "lifecyclevm",
        "--image",
        "alpine:latest",
        "--cpus",
        "1",
        "--memory",
        "128M",
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

    // -- Start --
    let start_output = common::ember(&["--state-dir", state, "vm", "start", "lifecyclevm"]);
    let start_stdout = String::from_utf8_lossy(&start_output.stdout);
    let start_stderr = String::from_utf8_lossy(&start_output.stderr);
    assert!(
        start_output.status.success(),
        "vm start failed.\nstdout: {start_stdout}\nstderr: {start_stderr}"
    );
    assert!(
        start_stdout.contains("started"),
        "expected 'started' in output: {start_stdout}"
    );

    // -- Verify Running via inspect --
    let inspect_output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "lifecyclevm",
        "--format",
        "json",
    ]);
    assert!(inspect_output.status.success());
    let inspect_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&inspect_output.stdout))
            .expect("failed to parse inspect JSON");
    assert_eq!(
        inspect_json["status"], "running",
        "expected status 'running', got: {}",
        inspect_json["status"]
    );

    // Verify the Firecracker process is actually alive.
    let pid = inspect_json["pid"]
        .as_u64()
        .expect("expected numeric PID in inspect output");
    let proc_alive = std::path::Path::new(&format!("/proc/{pid}")).exists();
    assert!(
        proc_alive,
        "expected Firecracker process (pid {pid}) to be alive"
    );

    // -- Stop --
    let stop_output =
        common::ember(&["--state-dir", state, "vm", "stop", "lifecyclevm", "--force"]);
    let stop_stdout = String::from_utf8_lossy(&stop_output.stdout);
    let stop_stderr = String::from_utf8_lossy(&stop_output.stderr);
    assert!(
        stop_output.status.success(),
        "vm stop failed.\nstdout: {stop_stdout}\nstderr: {stop_stderr}"
    );
    assert!(
        stop_stdout.contains("stopped"),
        "expected 'stopped' in output: {stop_stdout}"
    );

    // Verify status is Stopped.
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
    let inspect2_json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&inspect2.stdout))
            .expect("failed to parse inspect JSON after stop");
    assert_eq!(inspect2_json["status"], "stopped");
    assert!(
        inspect2_json["pid"].is_null(),
        "expected pid to be null after stop"
    );

    // Verify the process is dead.
    let proc_dead = !std::path::Path::new(&format!("/proc/{pid}")).exists();
    assert!(
        proc_dead,
        "expected Firecracker process (pid {pid}) to be dead after stop"
    );

    // -- Delete --
    let del_output = common::ember(&["--state-dir", state, "vm", "delete", "lifecyclevm"]);
    let del_stdout = String::from_utf8_lossy(&del_output.stdout);
    let del_stderr = String::from_utf8_lossy(&del_output.stderr);
    assert!(
        del_output.status.success(),
        "vm delete failed.\nstdout: {del_stdout}\nstderr: {del_stderr}"
    );

    // Verify zvol is gone.
    common::linux::assert_dataset_absent(&format!("{pool}/ember/vms/lifecyclevm"));

    // Verify VM no longer in list.
    let list_output = common::ember(&["--state-dir", state, "vm", "list"]);
    let list_stdout = String::from_utf8_lossy(&list_output.stdout);
    assert!(
        list_stdout.contains("No VMs found"),
        "expected empty vm list after delete: {list_stdout}"
    );
}

/// Delete a running VM requires --force.
///
/// Same prerequisites as `full_vm_lifecycle_start_stop_delete`.
#[test]
#[ignore]
fn delete_running_vm_requires_force() {
    if !common::linux::firecracker_available() {
        return;
    }

    let kernel_path = match common::linux::ensure_kernel() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_init_and_pull("vmdelrunning", &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // Create and start a VM.
    let create_output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "runningvm",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel,
        "--no-start",
    ]);
    assert!(
        create_output.status.success(),
        "vm create failed: {}",
        String::from_utf8_lossy(&create_output.stderr)
    );

    let start_output = common::ember(&["--state-dir", state, "vm", "start", "runningvm"]);
    assert!(
        start_output.status.success(),
        "vm start failed: {}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // Try to delete without --force — should fail.
    let del_output = common::ember(&["--state-dir", state, "vm", "delete", "runningvm"]);
    assert!(
        !del_output.status.success(),
        "expected delete of running VM to fail without --force"
    );
    let stderr = String::from_utf8_lossy(&del_output.stderr);
    assert!(
        stderr.contains("--force"),
        "expected error mentioning --force: {stderr}"
    );

    // Delete with --force — should succeed and kill the process.
    let force_output =
        common::ember(&["--state-dir", state, "vm", "delete", "runningvm", "--force"]);
    let force_stdout = String::from_utf8_lossy(&force_output.stdout);
    let force_stderr = String::from_utf8_lossy(&force_output.stderr);
    assert!(
        force_output.status.success(),
        "vm delete --force failed.\nstdout: {force_stdout}\nstderr: {force_stderr}"
    );

    // Verify zvol is gone.
    common::linux::assert_dataset_absent(&format!("{pool}/ember/vms/runningvm"));
}

// ---------------------------------------------------------------------------
// Networking test (requires Firecracker + kernel + network access)
// ---------------------------------------------------------------------------

/// Full networking test: start VM → verify TAP + iptables + host-to-guest ping
/// → SSH into guest → verify internet from guest → stop → delete.
///
/// Uses the `ubuntu-slim` image (built via Docker) which includes systemd,
/// openssh-server, and networking tools — everything needed for proper SSH
/// and internet connectivity testing.
///
/// Requires:
/// - `firecracker` in PATH, `/dev/kvm` available
/// - `docker` for building the ubuntu-slim image
/// - Bootable kernel (auto-downloaded or `EMBER_TEST_KERNEL`)
/// - SSH key pair for the invoking user
/// - Network access (host must be able to reach the internet)
///
/// Skips if prerequisites are missing.
#[test]
#[ignore]
fn networking_ssh_and_internet() {
    if !common::linux::firecracker_available() {
        return;
    }

    if !common::linux::docker_available() {
        eprintln!("Skipping: docker not available (needed to build ubuntu-slim image)");
        return;
    }

    let kernel_path = match common::linux::ensure_kernel() {
        Some(p) => p,
        None => return,
    };

    let ssh_key = match common::linux::ssh_private_key_path() {
        Some(p) => p,
        None => {
            eprintln!("Skipping: no SSH private key found for the invoking user");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        common::linux::setup_pool_init_and_build_ubuntu("vmnetwork", &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // -- Create VM --
    // Ubuntu with systemd needs more memory than Alpine with busybox init.
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
        tap_device.starts_with("em-"),
        "TAP device should start with 'em-', got: {tap_device}"
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
    // The guest needs a moment to boot and configure its network via the
    // kernel ip= parameter. Retry ping with short delays.
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
    // Ubuntu with systemd + sshd needs more time to boot than Alpine.
    eprintln!("Waiting for SSH to become available...");
    assert!(
        common::linux::wait_for_ssh(guest_ip, &ssh_key),
        "SSH not reachable at {guest_ip}:22 after timeout"
    );

    // Run a simple command to verify SSH exec works.
    let hostname_result = common::linux::ssh_exec(guest_ip, &ssh_key, "hostname");
    assert!(
        hostname_result.is_ok(),
        "SSH command 'hostname' failed: {:?}",
        hostname_result.err()
    );
    let hostname = hostname_result.unwrap();
    eprintln!("Guest hostname: {hostname}");

    // -- Verify DNS resolution from guest --
    // /etc/resolv.conf should be a symlink to /proc/net/pnp with real nameservers.
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

    // Verify DNS actually works by resolving a domain name.
    let dns_result = common::linux::ssh_exec(guest_ip, &ssh_key, "ping -c 1 -W 5 example.com");
    assert!(
        dns_result.is_ok(),
        "DNS resolution failed — guest cannot resolve example.com: {:?}",
        dns_result.err()
    );
    eprintln!("Guest DNS resolution verified (ping example.com)");

    // -- Verify internet from guest (requires both DNS + connectivity) --
    // Use curl with generous timeout — first DNS query after boot can be slow.
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
    eprintln!("Networking test complete.");
}

// ---------------------------------------------------------------------------
// Pause/Resume tests
// ---------------------------------------------------------------------------

/// Pausing a created (not running) VM should fail with a state error.
#[test]
#[ignore]
fn pause_created_vm_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_init_and_pull("vmpausecreated", &tmp);
    let kernel = common::linux::create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create a VM but don't start it.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "pausetest",
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

    // Try to pause — should fail because VM is in "created" state.
    let pause_output = common::ember(&["--state-dir", state, "vm", "pause", "pausetest"]);
    assert!(
        !pause_output.status.success(),
        "expected pause to fail for non-running VM"
    );
    let stderr = String::from_utf8_lossy(&pause_output.stderr);
    assert!(
        stderr.contains("created") && stderr.contains("expected running"),
        "expected state error mentioning 'created' and 'expected running': {stderr}"
    );
}

/// Resuming a created (not paused) VM should fail with a state error.
#[test]
#[ignore]
fn resume_created_vm_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (_pool, state_dir, _cleanup) =
        common::linux::setup_pool_init_and_pull("vmresumecreated", &tmp);
    let kernel = common::linux::create_dummy_kernel(tmp.path());
    let state = state_dir.to_str().unwrap();

    // Create a VM but don't start it.
    let output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "resumetest",
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

    // Try to resume — should fail because VM is in "created" state.
    let resume_output = common::ember(&["--state-dir", state, "vm", "resume", "resumetest"]);
    assert!(
        !resume_output.status.success(),
        "expected resume to fail for non-paused VM"
    );
    let stderr = String::from_utf8_lossy(&resume_output.stderr);
    assert!(
        stderr.contains("created") && stderr.contains("expected paused"),
        "expected state error mentioning 'created' and 'expected paused': {stderr}"
    );
}

/// Full pause/resume lifecycle: create → start → pause → verify paused
/// → resume → verify running → stop → delete.
///
/// Verifies:
/// - Pause transitions status from running to paused
/// - PID is preserved across pause (Firecracker process stays alive)
/// - Resume transitions status from paused to running
/// - PID is unchanged after resume
/// - Resuming a running VM (after resume) fails
/// - Pausing a paused VM fails
/// - Stopping a paused VM works
///
/// Requires `firecracker` in PATH and a bootable kernel.
#[test]
#[ignore]
fn pause_resume_lifecycle() {
    if !common::linux::firecracker_available() {
        return;
    }

    let kernel_path = match common::linux::ensure_kernel() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) =
        common::linux::setup_pool_init_and_pull("vmpauseresume", &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // -- Create --
    let create_output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "prvm",
        "--image",
        "alpine:latest",
        "--cpus",
        "1",
        "--memory",
        "128M",
        "--kernel",
        kernel,
        "--no-start",
    ]);
    assert!(
        create_output.status.success(),
        "vm create failed: {}",
        String::from_utf8_lossy(&create_output.stderr)
    );

    // -- Start --
    let start_output = common::ember(&["--state-dir", state, "vm", "start", "prvm"]);
    assert!(
        start_output.status.success(),
        "vm start failed: {}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // Capture PID while running.
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
    let pid = json1["pid"]
        .as_u64()
        .expect("expected numeric PID in inspect output");

    // -- Pause --
    let pause_output = common::ember(&["--state-dir", state, "vm", "pause", "prvm"]);
    let pause_stdout = String::from_utf8_lossy(&pause_output.stdout);
    let pause_stderr = String::from_utf8_lossy(&pause_output.stderr);
    assert!(
        pause_output.status.success(),
        "vm pause failed.\nstdout: {pause_stdout}\nstderr: {pause_stderr}"
    );
    assert!(
        pause_stdout.contains("paused"),
        "expected 'paused' in output: {pause_stdout}"
    );

    // Verify status is paused and PID is preserved.
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
    assert_eq!(
        json2["pid"].as_u64().unwrap(),
        pid,
        "PID should be preserved after pause"
    );

    // Firecracker process should still be alive (paused, not killed).
    assert!(
        Path::new(&format!("/proc/{pid}")).exists(),
        "Firecracker process (pid {pid}) should be alive while paused"
    );

    // -- Pausing an already paused VM should fail --
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

    // -- Resume --
    let resume_output = common::ember(&["--state-dir", state, "vm", "resume", "prvm"]);
    let resume_stdout = String::from_utf8_lossy(&resume_output.stdout);
    let resume_stderr = String::from_utf8_lossy(&resume_output.stderr);
    assert!(
        resume_output.status.success(),
        "vm resume failed.\nstdout: {resume_stdout}\nstderr: {resume_stderr}"
    );
    assert!(
        resume_stdout.contains("resumed"),
        "expected 'resumed' in output: {resume_stdout}"
    );

    // Verify status is back to running and PID is unchanged.
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
    assert_eq!(
        json3["pid"].as_u64().unwrap(),
        pid,
        "PID should be preserved after resume"
    );

    // -- Resuming a running VM should fail --
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

    // -- Stop and cleanup --
    let stop_output = common::ember(&["--state-dir", state, "vm", "stop", "prvm", "--force"]);
    assert!(
        stop_output.status.success(),
        "vm stop failed: {}",
        String::from_utf8_lossy(&stop_output.stderr)
    );

    let del_output = common::ember(&["--state-dir", state, "vm", "delete", "prvm"]);
    assert!(
        del_output.status.success(),
        "vm delete failed: {}",
        String::from_utf8_lossy(&del_output.stderr)
    );

    common::linux::assert_dataset_absent(&format!("{pool}/ember/vms/prvm"));
    eprintln!("Pause/resume lifecycle test complete.");
}

/// Stopping a paused VM should work (via --force).
///
/// Requires `firecracker` in PATH and a bootable kernel.
#[test]
#[ignore]
fn stop_paused_vm() {
    if !common::linux::firecracker_available() {
        return;
    }

    let kernel_path = match common::linux::ensure_kernel() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let (pool, state_dir, _cleanup) = common::linux::setup_pool_init_and_pull("vmstoppaused", &tmp);
    let state = state_dir.to_str().unwrap();
    let kernel = kernel_path.to_str().unwrap();

    // Create and start.
    let create_output = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "create",
        "spvm",
        "--image",
        "alpine:latest",
        "--cpus",
        "1",
        "--memory",
        "128M",
        "--kernel",
        kernel,
        "--no-start",
    ]);
    assert!(
        create_output.status.success(),
        "vm create failed: {}",
        String::from_utf8_lossy(&create_output.stderr)
    );

    let start_output = common::ember(&["--state-dir", state, "vm", "start", "spvm"]);
    assert!(
        start_output.status.success(),
        "vm start failed: {}",
        String::from_utf8_lossy(&start_output.stderr)
    );

    // Get PID for later verification.
    let inspect = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "spvm",
        "--format",
        "json",
    ]);
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect.stdout))
        .expect("failed to parse inspect JSON");
    let pid = json["pid"].as_u64().expect("expected numeric PID");

    // Pause the VM.
    let pause_output = common::ember(&["--state-dir", state, "vm", "pause", "spvm"]);
    assert!(
        pause_output.status.success(),
        "vm pause failed: {}",
        String::from_utf8_lossy(&pause_output.stderr)
    );

    // Stop the paused VM with --force.
    let stop_output = common::ember(&["--state-dir", state, "vm", "stop", "spvm", "--force"]);
    let stop_stdout = String::from_utf8_lossy(&stop_output.stdout);
    let stop_stderr = String::from_utf8_lossy(&stop_output.stderr);
    assert!(
        stop_output.status.success(),
        "vm stop --force failed for paused VM.\nstdout: {stop_stdout}\nstderr: {stop_stderr}"
    );

    // Verify process is dead.
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "Firecracker process (pid {pid}) should be dead after stop"
    );

    // Verify status is stopped.
    let inspect2 = common::ember(&[
        "--state-dir",
        state,
        "vm",
        "inspect",
        "spvm",
        "--format",
        "json",
    ]);
    assert!(inspect2.status.success());
    let json2: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&inspect2.stdout))
        .expect("failed to parse inspect JSON after stop");
    assert_eq!(json2["status"], "stopped");

    // Cleanup.
    let del_output = common::ember(&["--state-dir", state, "vm", "delete", "spvm"]);
    assert!(
        del_output.status.success(),
        "vm delete failed: {}",
        String::from_utf8_lossy(&del_output.stderr)
    );

    common::linux::assert_dataset_absent(&format!("{pool}/ember/vms/spvm"));
    eprintln!("Stop paused VM test complete.");
}
