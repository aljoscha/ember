//! macOS VM backend: Apple Virtualization Framework via ember-vz helper.
//!
//! Spawns and signals the `ember-vz` Swift helper process to manage VMs.
//! The helper communicates back via a ready-fd pipe (writes the guest MAC
//! address once the VM is booted) and responds to Unix signals for lifecycle
//! control (SIGTERM, SIGUSR1, SIGUSR2).
//!
//! **Start flow**: spawns `ember-vz start` with kernel, disk, CPU/memory,
//! and a ready-fd pipe. Reads the MAC address from the pipe once the VM
//! boots. The MAC is stored in `NetworkInfo.guest_mac` so that Phase 4
//! networking can use it to discover the guest IP from vmnet DHCP leases.

use std::io::{BufRead, BufReader};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::process::{Command, Stdio};
use std::time::Duration;

use nix::libc;

use crate::backend::{StartedVm, VmBackend};
use crate::cli::init::GlobalConfig;
use crate::error::{Error, Result};
use crate::state::vm::{NetworkInfo, VmMetadata};

/// macOS VM backend using Apple Virtualization Framework (via ember-vz).
pub struct MacosVm;

/// Default boot args for AVF guests.
/// Uses `console=hvc0` (virtio console) instead of Linux's `console=ttyS0`.
const DEFAULT_BOOT_ARGS: &str = "console=hvc0 root=/dev/vda rw";

/// Timeout waiting for ember-vz to report VM readiness via ready-fd.
/// AVF boot is typically fast (a few seconds), but allow headroom for
/// slow disks or large kernels.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// Name of the Swift helper binary.
const EMBER_VZ_BIN: &str = "ember-vz";

/// Timeout for graceful VM shutdown (SIGTERM) before falling back to SIGKILL.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for SIGKILL to take effect.
const FORCE_KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval when waiting for a process to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

impl VmBackend for MacosVm {
    /// Start a VM by spawning the `ember-vz` helper process.
    ///
    /// Creates a pipe for ready-fd communication, spawns `ember-vz start`
    /// with the VM's kernel, disk image, CPU/memory config, and boot args.
    /// Waits for the helper to write the guest MAC address to the pipe,
    /// indicating the VM has booted successfully.
    ///
    /// Returns the helper's PID and a NetworkInfo containing the guest MAC.
    /// Guest IP discovery (via vmnet DHCP leases) is handled separately
    /// by the network backend.
    fn start(vm: &VmMetadata, config: &GlobalConfig) -> Result<StartedVm> {
        // Derive paths for the VM's serial console log.
        // The log lives next to vm.json in the VM directory.
        let vm_dir = config.state_dir.join("vms").join(&vm.name);
        let serial_log = vm_dir.join("console.log");

        // Boot args: use custom if set, otherwise the AVF default.
        let boot_args = vm.boot_args.as_deref().unwrap_or(DEFAULT_BOOT_ARGS);

        // Create a pipe for ready-fd communication.
        // ember-vz writes the guest MAC address to the write end once booted;
        // we read it from the read end.
        let (read_owned, write_owned) =
            nix::unistd::pipe().map_err(|e| Error::Network(format!("pipe: {e}")))?;

        // Convert OwnedFd to raw fds for use with from_raw_fd / pre_exec.
        let read_raw = read_owned.into_raw_fd();
        let write_raw = write_owned.into_raw_fd();

        // SAFETY: We just created write_raw above and it's a valid open fd.
        // We wrap it in a File so it gets closed when dropped (after spawn).
        let write_file = unsafe { std::fs::File::from_raw_fd(write_raw) };
        let write_fd_num = write_file.as_raw_fd();

        // Build the ember-vz start command.
        let mut cmd = Command::new(EMBER_VZ_BIN);
        cmd.arg("start")
            .arg("--kernel")
            .arg(&vm.kernel_path)
            .arg("--disk")
            .arg(&vm.zvol_path)
            .arg("--cpus")
            .arg(vm.cpus.to_string())
            .arg("--memory")
            .arg(vm.memory_mib.to_string())
            .arg("--boot-args")
            .arg(boot_args)
            .arg("--network")
            .arg("shared")
            .arg("--serial-log")
            .arg(&serial_log)
            .arg("--ready-fd")
            .arg(write_fd_num.to_string());

        // Redirect stdout/stderr to the serial log / null so the helper
        // doesn't interfere with ember's terminal output.
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        // SAFETY: pre_exec runs between fork and exec. We clear the
        // close-on-exec flag on the write fd so ember-vz inherits it.
        // No allocations or async-signal-unsafe calls here.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                // Clear CLOEXEC so the child inherits this fd.
                let flags = libc::fcntl(write_fd_num, libc::F_GETFD);
                if flags < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(write_fd_num, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        // Spawn the helper process.
        let child = cmd.spawn().map_err(|e| Error::CommandExec {
            command: EMBER_VZ_BIN.to_string(),
            source: e,
        })?;
        let pid = child.id();

        // Close the write end in the parent — only the child writes to it.
        drop(write_file);

        // Read the MAC address from the ready-fd pipe.
        // ember-vz writes "<MAC>\n" once the VM has booted.
        let mac = match read_mac_from_ready_fd(read_raw, READY_TIMEOUT) {
            Ok(mac) => mac,
            Err(e) => {
                // Boot failed or timed out — kill the orphaned helper.
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                return Err(e);
            }
        };

        // Build network info with the MAC address from the helper.
        // Guest IP and other fields are populated later by NetworkBackend
        // once DHCP lease discovery is implemented (Phase 4).
        let network = if let Some(existing) = &vm.network {
            // Preserve any info from network setup, add the MAC.
            NetworkInfo {
                guest_mac: Some(mac),
                ..existing.clone()
            }
        } else {
            // No network setup yet — create minimal info with just the MAC.
            // vmnet shared mode provides the gateway at 192.168.64.1 by default.
            NetworkInfo {
                tap_device: String::new(),
                host_ip: String::new(),
                guest_ip: String::new(),
                netmask: String::new(),
                guest_mac: Some(mac),
                wan_iface: None,
            }
        };

        Ok(StartedVm { pid, network })
    }

    /// Graceful stop: send SIGTERM to ember-vz, wait for exit, SIGKILL fallback.
    ///
    /// SIGTERM triggers `VZVirtualMachine.stop()` in the helper, which performs
    /// a clean ACPI shutdown. If the process doesn't exit within the timeout,
    /// we escalate to SIGKILL.
    fn stop(vm: &VmMetadata) -> Result<()> {
        let pid = vm
            .pid
            .ok_or_else(|| Error::Network(format!("vm '{}' has no PID", vm.name)))?;

        if !Self::is_running(pid) {
            return Ok(());
        }

        // Send SIGTERM for graceful shutdown.
        let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
        nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM).map_err(|e| {
            Error::Network(format!(
                "failed to send SIGTERM to ember-vz (pid {pid}): {e}"
            ))
        })?;

        // Wait for the process to exit.
        if !wait_for_exit(pid, GRACEFUL_SHUTDOWN_TIMEOUT) {
            // Still alive — escalate to SIGKILL.
            let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGKILL);
            wait_for_exit(pid, FORCE_KILL_TIMEOUT);
        }

        Ok(())
    }

    /// Force stop: send SIGKILL immediately.
    fn force_stop(vm: &VmMetadata) -> Result<()> {
        let pid = vm
            .pid
            .ok_or_else(|| Error::Network(format!("vm '{}' has no PID", vm.name)))?;

        if !Self::is_running(pid) {
            return Ok(());
        }

        let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
        nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGKILL).map_err(|e| {
            Error::Network(format!(
                "failed to send SIGKILL to ember-vz (pid {pid}): {e}"
            ))
        })?;

        wait_for_exit(pid, FORCE_KILL_TIMEOUT);
        Ok(())
    }

    fn pause(_vm: &VmMetadata) -> Result<()> {
        todo!("macOS: pause VM (SIGUSR1 to ember-vz)")
    }

    fn resume(_vm: &VmMetadata) -> Result<()> {
        todo!("macOS: resume VM (SIGUSR2 to ember-vz)")
    }

    fn is_running(pid: u32) -> bool {
        // kill(pid, 0) works the same on macOS as Linux.
        unsafe { nix::libc::kill(pid as i32, 0) == 0 }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Wait for a process to exit, polling `kill(pid, 0)` at regular intervals.
///
/// Returns `true` if the process exited within the timeout, `false` if still alive.
fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if !MacosVm::is_running(pid) {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    !MacosVm::is_running(pid)
}

/// Read the guest MAC address from the ready-fd pipe with a timeout.
///
/// The ember-vz helper writes `<MAC>\n` to the pipe once the VM has
/// successfully booted. We use a poll-based approach with a timeout
/// to avoid blocking forever if the VM fails to start.
fn read_mac_from_ready_fd(read_fd: i32, timeout: Duration) -> Result<String> {
    // SAFETY: read_fd is a valid fd from nix::unistd::pipe().
    // We wrap it in a File for automatic cleanup via Drop.
    let read_file = unsafe { std::fs::File::from_raw_fd(read_fd) };

    // Use poll() to wait for data with a timeout, so we don't block
    // forever if ember-vz crashes before writing the MAC.
    let mut pollfd = libc::pollfd {
        fd: read_file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };

    let timeout_ms = timeout.as_millis() as i32;
    // SAFETY: pollfd is a valid stack-allocated struct, nfds=1.
    let poll_result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };

    if poll_result < 0 {
        return Err(Error::Network(format!(
            "poll on ready-fd failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    if poll_result == 0 {
        return Err(Error::Network(format!(
            "timed out waiting for ember-vz to report VM readiness ({}s)",
            timeout.as_secs()
        )));
    }

    // Data is available — read the MAC address line.
    let mut reader = BufReader::new(read_file);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| Error::Network(format!("failed to read MAC from ready-fd: {e}")))?;

    let mac = line.trim().to_string();
    if mac.is_empty() {
        return Err(Error::Network(
            "ember-vz closed ready-fd without writing MAC address (VM may have crashed)".into(),
        ));
    }

    Ok(mac)
}
