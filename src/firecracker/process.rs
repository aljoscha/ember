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

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn a Firecracker process.
///
/// Starts `firecracker` with the given API socket and log file paths.
/// Returns the `Child` handle — the caller should extract `.id()` for
/// the PID and store it in VM metadata.
///
/// Guest serial console output (`console=ttyS0`) is captured to a
/// `console.log` file next to the Firecracker log. Stderr goes to
/// `/dev/null`; Firecracker writes its own operational log via `--log-path`.
pub fn spawn(socket_path: &Path, log_path: &Path) -> anyhow::Result<Child> {
    let console_log_path = log_path.with_file_name("console.log");
    let console_log = File::create(&console_log_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to create console log at {}: {e}",
            console_log_path.display()
        )
    })?;

    let child = Command::new("firecracker")
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
                "failed to spawn firecracker: {e}\n\
                 Hint: is the 'firecracker' binary installed and in PATH?"
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
