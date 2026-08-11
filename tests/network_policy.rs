//! Firewall policy contract for the Linux backend.
//!
//! Two promises are under test:
//!
//! 1. A VM can reach the other VMs of its own installation.
//! 2. A VM cannot reach the host, at any of the host's addresses.
//!
//! Both used to be accidents of whatever else was in the host's
//! iptables policy, which is why the structural test below pins down
//! rule *placement* and not just rule presence. A terminal DROP that
//! drifts above the per-VM ACCEPTs, or a jump that stops being first in
//! INPUT, turns the contract back into a coin flip while every rule is
//! still technically there.
//!
//! Gated `#[ignore]` and Linux-only because they touch real iptables,
//! TAP and hypervisor state. Run explicitly with:
//!
//! ```text
//! sudo cargo test --test network_policy -- --ignored --test-threads=1
//! ```

#![cfg(target_os = "linux")]
#![allow(clippy::zombie_processes)]

#[allow(dead_code)]
mod common;

use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------------------
// iptables helpers
// ---------------------------------------------------------------------------

fn iptables(args: &[&str]) -> Result<String, String> {
    let output = Command::new("iptables")
        .arg("-w")
        .arg("5")
        .args(args)
        .output()
        .expect("failed to run iptables");
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Rules in a chain, in order, as `iptables -S` prints them. Empty when
/// the chain does not exist.
fn rules(chain: &str) -> Vec<String> {
    match iptables(&["-S", chain]) {
        Ok(out) => out
            .lines()
            .filter(|l| l.starts_with("-A"))
            .map(|l| l.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn chain_exists(chain: &str) -> bool {
    iptables(&["-S", chain]).is_ok()
}

/// Stops the test's VMs on the way out, including on a panic.
///
/// Without this, a Firecracker process outlives the test and holds its
/// zvol open, and the harness's pool teardown blocks in `zpool destroy`
/// in uninterruptible sleep until the process is killed by hand. Must
/// be declared *after* the `TestEnv` so it drops before the pool does.
struct VmCleanup {
    state: String,
    names: Vec<&'static str>,
}

impl Drop for VmCleanup {
    fn drop(&mut self) {
        for name in &self.names {
            common::stop_and_delete_vm(&self.state, name);
        }
    }
}

// ---------------------------------------------------------------------------
// State helpers
// ---------------------------------------------------------------------------

fn read_json(path: &Path) -> serde_json::Value {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("malformed json")
}

/// The install's instance id, which every chain and TAP name derives
/// from.
fn instance_id(state_dir: &str) -> String {
    read_json(&Path::new(state_dir).join("config.json"))["instance_id"]
        .as_str()
        .expect("config has no instance_id")
        .to_string()
}

/// A running VM's persisted network info.
fn network_info(state_dir: &str, vm: &str) -> serde_json::Value {
    read_json(&Path::new(state_dir).join("vms").join(vm).join("vm.json"))["network"].clone()
}

fn field(value: &serde_json::Value, key: &str) -> String {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("network info has no {key}: {value}"))
        .to_string()
}

// ---------------------------------------------------------------------------
// Structural test: one alpine VM, no guest tooling needed
// ---------------------------------------------------------------------------

/// The chains exist, they are entered first, the per-VM rules live
/// inside them, the terminal DROP stays last, and `deinit` takes it all
/// away again.
#[test]
#[ignore = "requires root + firecracker + a ZFS pool"]
fn policy_chains_are_placed_correctly_and_removed_on_deinit() {
    let env = common::TestEnv::with_running_vm("netpolicy", "polvm");
    let state = env.state().to_string();

    let id = instance_id(&state);
    let input_chain = format!("ember-{id}-input");
    let forward_chain = format!("ember-{id}-forward");
    let taps = format!("em{id}-+");
    let _vm_cleanup = VmCleanup {
        state: state.clone(),
        names: vec!["polvm"],
    };

    let net = network_info(&state, "polvm");
    let tap = field(&net, "tap_device");
    let wan = field(&net, "wan_iface");
    assert_eq!(
        field(&net, "firewall_chain"),
        forward_chain,
        "the VM must record which chain its rules went into, or an \
         upgraded binary cannot delete them"
    );

    // ── Chains and jumps ─────────────────────────────────────────

    assert!(chain_exists(&input_chain), "{input_chain} was not created");
    assert!(
        chain_exists(&forward_chain),
        "{forward_chain} was not created"
    );

    for (builtin, chain) in [("INPUT", &input_chain), ("FORWARD", &forward_chain)] {
        let first = rules(builtin).first().cloned().unwrap_or_default();
        assert_eq!(
            first,
            format!("-A {builtin} -j {chain}"),
            "the jump into {chain} must be the first rule in {builtin}, \
             otherwise a pre-existing ACCEPT can match guest traffic first"
        );
    }

    // ── Static policy ────────────────────────────────────────────

    let forward = rules(&forward_chain);
    assert!(
        forward.contains(&format!("-A {forward_chain} -i {taps} -o {taps} -j ACCEPT")),
        "missing the VM-to-VM rule in {forward_chain}: {forward:#?}"
    );
    assert_eq!(
        forward.last().cloned().unwrap_or_default(),
        format!("-A {forward_chain} -i {taps} -j DROP"),
        "the terminal DROP must be the last rule in {forward_chain}, or it \
         shadows the per-VM ACCEPTs above it: {forward:#?}"
    );

    let input = rules(&input_chain);
    let established = input
        .iter()
        .position(|r| r.contains("RELATED,ESTABLISHED"))
        .unwrap_or_else(|| panic!("no established-accept in {input_chain}: {input:#?}"));
    let drop = input
        .iter()
        .position(|r| r.ends_with("-j DROP"))
        .unwrap_or_else(|| panic!("no host block in {input_chain}: {input:#?}"));
    assert!(
        established < drop,
        "established traffic must be accepted before the host block, or \
         `ember ssh` breaks: {input:#?}"
    );

    // ── Per-VM rules ─────────────────────────────────────────────

    assert!(
        forward.contains(&format!("-A {forward_chain} -i {tap} -o {wan} -j ACCEPT")),
        "missing the VM's egress rule in {forward_chain}: {forward:#?}"
    );
    assert!(
        !rules("FORWARD").iter().any(|r| r.contains(&tap)),
        "per-VM rules must live in {forward_chain}, not the built-in FORWARD chain"
    );

    // ── Stop: per-VM rules go, the install's policy stays ─────────

    let output = common::ember(&["--state-dir", &state, "vm", "stop", "polvm"]);
    assert!(
        output.status.success(),
        "vm stop failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let forward = rules(&forward_chain);
    assert!(
        !forward.iter().any(|r| r.contains(&tap)),
        "the stopped VM's rules were left behind: {forward:#?}"
    );
    assert!(
        forward.iter().any(|r| r.ends_with("-j DROP")),
        "the install's policy must outlive its VMs: {forward:#?}"
    );

    // ── Deinit: everything goes ──────────────────────────────────

    let output = common::ember(&["--state-dir", &state, "vm", "delete", "polvm"]);
    assert!(
        output.status.success(),
        "vm delete failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = common::ember(&["--state-dir", &state, "deinit"]);
    assert!(
        output.status.success(),
        "deinit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(!chain_exists(&input_chain), "{input_chain} survived deinit");
    assert!(
        !chain_exists(&forward_chain),
        "{forward_chain} survived deinit"
    );
    // Scoped to this install's chain names. The developer's own install
    // has its chains in INPUT and FORWARD too, and they legitimately
    // outlive its VMs, so anything looser than an exact name here fails
    // on every machine that actually runs ember.
    for (builtin, chain) in [("INPUT", &input_chain), ("FORWARD", &forward_chain)] {
        assert!(
            !rules(builtin).iter().any(|r| r.contains(chain.as_str())),
            "the jump into {chain} survived deinit in {builtin}"
        );
    }
}

// ---------------------------------------------------------------------------
// Connectivity test: two ubuntu VMs, real traffic
// ---------------------------------------------------------------------------

/// Run a command inside a VM and return the exit status it had *in the
/// guest*.
///
/// The status is echoed and parsed back rather than taken from `ember
/// exec`, because the negative assertions below depend on telling "the
/// host was unreachable" apart from "we never got to try". Those look
/// identical if the exec path's own failure is read as the command
/// failing. A missing marker panics instead of reporting a status.
fn guest_status(state: &str, vm: &str, command: &str) -> i32 {
    let script = format!("{command} >/dev/null 2>&1; echo EXIT:$?");
    let output = common::ember(&["--state-dir", state, "exec", vm, "--", "sh", "-c", &script]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("EXIT:")?.parse::<i32>().ok())
        .unwrap_or_else(|| {
            panic!(
                "`{command}` never ran in '{vm}'\nstdout: {stdout}\nstderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
}

/// Whether a guest can reach `target`, retrying while the peer VM is
/// still booting. Only useful for the positive direction: a blocked
/// target would burn the whole window.
fn reachable_within(state: &str, vm: &str, target: &str, tries: u32) -> bool {
    for _ in 0..tries {
        if guest_status(state, vm, &format!("ping -c1 -W5 {target}")) == 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    false
}

/// The contract itself, with real packets: siblings reachable, host
/// not, internet still fine.
#[test]
#[ignore = "requires root + firecracker + docker + internet; boots two VMs"]
fn vms_reach_each_other_but_not_the_host() {
    let env = common::TestEnv::with_running_ssh_vm("netpolicyconn", "vma");
    let state = env.state().to_string();

    let _vm_cleanup = VmCleanup {
        state: state.clone(),
        names: vec!["vma", "vmb"],
    };

    // A second VM in the same install, so the two are siblings. Alpine
    // rather than the ubuntu-slim image vma runs: this VM only has to
    // answer pings, which the guest kernel does on its own, and the
    // harness sizes its pool for exactly one ubuntu rootfs.
    let output = common::ember(&["--state-dir", &state, "image", "pull", "alpine:latest"]);
    assert!(
        output.status.success(),
        "pulling alpine failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let kernel = common::linux::ensure_kernel();
    let output = common::ember(&[
        "--state-dir",
        &state,
        "vm",
        "create",
        "vmb",
        "--image",
        "alpine:latest",
        "--kernel",
        kernel.to_str().unwrap(),
        "--cpus",
        "1",
        "--memory",
        "128M",
    ]);
    assert!(
        output.status.success(),
        "creating the second VM failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let net_a = network_info(&state, "vma");
    let net_b = network_info(&state, "vmb");
    let guest_b = field(&net_b, "guest_ip");
    let host_ip_a = field(&net_a, "host_ip");

    // Sibling reachability. The two VMs are on different /30 links, so
    // this only works if the host forwards between their TAPs. Retried,
    // because `vm create` returns once Firecracker is up rather than
    // once the guest has finished booting.
    assert!(
        reachable_within(&state, "vma", &guest_b, 20),
        "VM A could not reach sibling VM B at {guest_b}"
    );

    // The host, at the guest's own default gateway. This is the address
    // a guest is most likely to poke at, and the one it must not reach.
    assert_ne!(
        guest_status(&state, "vma", &format!("ping -c1 -W5 {host_ip_a}")),
        0,
        "VM A reached the host at its gateway {host_ip_a}"
    );

    // Every other host address is equally off limits, including the
    // gateway of a sibling's link.
    let host_ip_b = field(&net_b, "host_ip");
    assert_ne!(
        guest_status(&state, "vma", &format!("ping -c1 -W5 {host_ip_b}")),
        0,
        "VM A reached the host at {host_ip_b}, the block must cover every \
         host address and not just the VM's own gateway"
    );

    // Egress still works. The terminal DROP is the most likely thing to
    // have broken it.
    assert_eq!(
        guest_status(&state, "vma", "ping -c1 -W10 1.1.1.1"),
        0,
        "VM A lost outbound connectivity"
    );
}
