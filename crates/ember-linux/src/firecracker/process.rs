//! Firecracker process management: spawn, wait, kill.
//!
//! Manages the Firecracker VMM process lifecycle. The process is spawned
//! as a child with `--api-sock` and `--log-path`, then controlled via
//! the API socket and Unix signals.

use std::fs::File;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Extra MiB allowed above the guest's allocation before the cgroup's
/// `MemoryMax` kicks in. Covers Firecracker's own RSS plus KVM/vsock
/// bookkeeping; a runaway guest still trips the limit and the kernel
/// OOM-kills the scope instead of the host.
const MEMORY_HEADROOM_MIB: u32 = 128;

/// Spawn a Firecracker process inside a transient `systemd-run --scope`.
///
/// The scope applies cgroup limits derived from `vcpu_count` and
/// `mem_size_mib` so a runaway guest can't take the host down with it:
/// `MemoryMax` caps RSS, `MemorySwapMax=0` keeps the host out of swap
/// thrash, `CPUQuota` caps CPU at N cores, and `CPUWeight=50` /
/// `IOWeight=50` halve the scope's share of contended CPU and disk
/// against other cgroups so the host stays responsive when the guest
/// is busy.
///
/// `systemd-run --scope` execve-replaces itself with `firecracker` after
/// the scope is set up, so `Child::id()` is the firecracker PID directly —
/// the rest of the process module (kill, is_alive, wait_for_exit) keeps
/// working unchanged. When firecracker exits the scope's last task is
/// gone and systemd reaps the transient unit on its own.
///
/// Guest serial console output (`console=ttyS0`) is captured to a
/// `console.log` file next to the Firecracker log. Stderr goes to
/// `/dev/null`; Firecracker writes its own operational log via `--log-path`.
pub fn spawn(
    socket_path: &Path,
    log_path: &Path,
    vcpu_count: u32,
    mem_size_mib: u32,
) -> anyhow::Result<Child> {
    let console_log_path = log_path.with_file_name("console.log");
    let console_log = File::create(&console_log_path).with_context(|| {
        format!(
            "failed to create console log at {}",
            console_log_path.display()
        )
    })?;

    let mem_max_mib = mem_size_mib + MEMORY_HEADROOM_MIB;
    let cpu_quota_pct = vcpu_count * 100;

    let scope_args: [String; 14] = [
        "--scope".into(),
        "--quiet".into(),
        "--slice=ember.slice".into(),
        "-p".into(),
        format!("MemoryMax={mem_max_mib}M"),
        "-p".into(),
        "MemorySwapMax=0".into(),
        "-p".into(),
        format!("CPUQuota={cpu_quota_pct}%"),
        "-p".into(),
        "CPUWeight=50".into(),
        "-p".into(),
        "IOWeight=50".into(),
        "--".into(),
    ];

    let child = Command::new("systemd-run")
        .args(scope_args)
        .arg("firecracker")
        .arg("--api-sock")
        .arg(socket_path)
        .arg("--log-path")
        .arg(log_path)
        .arg("--level")
        .arg("Info")
        .stdin(Stdio::null())
        .stdout(console_log)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to spawn firecracker under systemd-run: {e}\n\
                 Hint: are 'systemd-run' and 'firecracker' installed and in PATH?"
            )
        })?;

    Ok(child)
}

/// Wait for the Firecracker API socket to appear.
///
/// Polls every 10ms until the socket file exists, with a 5-second timeout.
/// Call this after [`spawn`] before making any API calls.
pub fn wait_for_socket(socket_path: &Path) -> anyhow::Result<()> {
    let start = Instant::now();
    while start.elapsed() < SOCKET_TIMEOUT {
        if socket_path.exists() {
            return Ok(());
        }
        thread::sleep(SOCKET_POLL_INTERVAL);
    }
    anyhow::bail!(
        "firecracker API socket did not appear at {} within {:?}\n\
         Hint: check {} for errors",
        socket_path.display(),
        SOCKET_TIMEOUT,
        socket_path.with_extension("log").display(),
    )
}

/// Check whether a process with the given PID is still alive.
///
/// Uses `kill(pid, 0)` which checks for process existence without
/// sending a signal.
pub fn is_alive(pid: u32) -> bool {
    signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

/// Send SIGKILL to a process.
///
/// Succeeds silently if the process is already dead (ESRCH).
pub fn kill(pid: u32) -> anyhow::Result<()> {
    match signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => anyhow::bail!("failed to kill firecracker process (pid {pid}): {e}"),
    }
}

/// Wait for a process to exit within the given timeout.
///
/// Polls at 50ms intervals. Returns `true` if the process exited before
/// the deadline, `false` if it is still alive.
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !is_alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !is_alive(pid)
}
