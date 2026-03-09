//! Integration tests for the macOS VM backend (Phase 3).
//!
//! Exercises the full VM lifecycle via the `ember-vz` helper: start with
//! ready-fd pipe (MAC address), is_running check, pause/resume via signals,
//! and graceful/forceful stop.
//!
//! Requirements:
//! - macOS 13+ with Virtualization.framework
//! - `ember-vz` binary built (`swift build` in ember-vz/)
//! - AVF-compatible kernel (see `ensure_kernel()` resolution order)
//! - Homebrew `e2fsprogs` (for mkfs.ext4)
//! - No root required
//!
//! Note: SSH testing requires Phase 4 networking (guest IP discovery
//! from vmnet DHCP leases). Those tests will be in the Phase 4 suite.
//!
//! To run:
//!   ./run-integration-tests.sh macos_vm
#![cfg(target_os = "macos")]

use std::io::{BufRead, BufReader};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const KERNEL_CACHE_PATH: &str = "/tmp/ember-test-vmlinux";
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Locate the `ember-vz` binary.
fn ember_vz_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("EMBER_VZ") {
        let path = PathBuf::from(&p);
        assert!(path.exists(), "EMBER_VZ={p} does not exist");
        return Some(path);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for subdir in ["release", "debug"] {
        let path = manifest_dir.join(format!("ember-vz/.build/{subdir}/ember-vz"));
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Get an AVF-compatible kernel.
fn ensure_kernel() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("EMBER_TEST_KERNEL") {
        let path = PathBuf::from(&p);
        assert!(path.exists(), "EMBER_TEST_KERNEL={p} does not exist");
        return Some(path);
    }

    let cache = PathBuf::from(KERNEL_CACHE_PATH);
    if cache.exists() {
        return Some(cache);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let local_kernel = manifest_dir.join("kernel/vmlinux");
    if local_kernel.exists() {
        let _ = std::fs::copy(&local_kernel, &cache);
        return Some(cache);
    }

    eprintln!(
        "Skipping: no AVF-compatible kernel found.\n\
         Build one with: cd kernel && make docker-build ARCH=arm64 FRAGMENTS=\"avf.fragment\""
    );
    None
}

fn find_e2fsprogs_tool(name: &str) -> String {
    for prefix in [
        "/opt/homebrew/opt/e2fsprogs/sbin",
        "/usr/local/opt/e2fsprogs/sbin",
    ] {
        let path = format!("{prefix}/{name}");
        if Path::new(&path).exists() {
            return path;
        }
    }
    name.to_string()
}

fn create_test_rootfs(dir: &Path, size_mb: u64) -> PathBuf {
    let img = dir.join("rootfs.img");

    let status = Command::new("truncate")
        .args(["-s", &format!("{size_mb}M")])
        .arg(&img)
        .status()
        .expect("failed to run truncate");
    assert!(status.success(), "truncate failed");

    let mkfs = find_e2fsprogs_tool("mkfs.ext4");
    let output = Command::new(&mkfs)
        .args(["-F", "-q"])
        .arg(&img)
        .output()
        .unwrap_or_else(|_| {
            panic!("failed to run {mkfs} — is e2fsprogs installed? (brew install e2fsprogs)")
        });
    assert!(
        output.status.success(),
        "mkfs.ext4 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    img
}

/// Check if a process is alive via kill(pid, 0).
fn is_running(pid: u32) -> bool {
    unsafe { nix::libc::kill(pid as i32, 0) == 0 }
}

/// Spawn ember-vz with a ready-fd pipe.
///
/// Returns (child, pid, read_fd) where read_fd is the parent's end of
/// the pipe that ember-vz writes the MAC address to.
fn spawn_ember_vz(
    ember_vz: &Path,
    kernel: &Path,
    rootfs: &Path,
    serial_log: &Path,
) -> (std::process::Child, u32, std::fs::File) {
    // Create ready-fd pipe.
    let (read_owned, write_owned) = nix::unistd::pipe().expect("pipe failed");
    let read_raw = read_owned.into_raw_fd();
    let write_raw = write_owned.into_raw_fd();
    let write_file = unsafe { std::fs::File::from_raw_fd(write_raw) };
    let write_fd_num = write_file.as_raw_fd();

    let mut cmd = Command::new(ember_vz);
    cmd.args([
        "start",
        "--kernel",
        kernel.to_str().unwrap(),
        "--disk",
        rootfs.to_str().unwrap(),
        "--cpus",
        "1",
        "--memory",
        "256",
        "--boot-args",
        "console=hvc0 root=/dev/vda rw",
        "--serial-log",
        serial_log.to_str().unwrap(),
        "--ready-fd",
        &write_fd_num.to_string(),
    ]);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    // Clear CLOEXEC on the write fd so ember-vz inherits it.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            let flags = nix::libc::fcntl(write_fd_num, nix::libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::fcntl(
                write_fd_num,
                nix::libc::F_SETFD,
                flags & !nix::libc::FD_CLOEXEC,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().expect("failed to spawn ember-vz");
    let pid = child.id();

    // Close write end in parent.
    drop(write_file);

    // Wrap read end in a File for the caller.
    let read_file = unsafe { std::fs::File::from_raw_fd(read_raw) };

    (child, pid, read_file)
}

/// Read MAC address from the ready-fd pipe with a timeout.
fn read_mac_from_pipe(read_file: std::fs::File, timeout: Duration) -> Option<String> {
    let mut pollfd = nix::libc::pollfd {
        fd: read_file.as_raw_fd(),
        events: nix::libc::POLLIN,
        revents: 0,
    };

    let result = unsafe { nix::libc::poll(&mut pollfd, 1, timeout.as_millis() as i32) };

    if result <= 0 {
        return None;
    }

    let mut reader = BufReader::new(read_file);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }

    let mac = line.trim().to_string();
    if mac.is_empty() {
        None
    } else {
        Some(mac)
    }
}

/// Wait for process to exit within timeout, SIGKILL fallback.
fn wait_for_exit(child: &mut std::process::Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {
                if start.elapsed() > timeout {
                    eprintln!("Timeout — sending SIGKILL");
                    let _ = child.kill();
                    return child.wait().expect("wait after kill failed");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("error waiting for process: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full VM lifecycle: start (with ready-fd) → is_running → stop (SIGTERM).
///
/// This tests the same flow as `MacosVm::start` / `MacosVm::stop`:
/// 1. Spawn ember-vz with --ready-fd pipe
/// 2. Read MAC address from pipe (proves VM booted)
/// 3. Verify process is running via kill(pid, 0)
/// 4. Send SIGTERM for graceful shutdown
/// 5. Verify clean exit
#[test]
#[ignore]
fn vm_lifecycle_start_stop() {
    let ember_vz = match ember_vz_bin() {
        Some(p) => {
            eprintln!("Using ember-vz: {}", p.display());
            p
        }
        None => {
            eprintln!("Skipping: ember-vz not found");
            return;
        }
    };

    let kernel = match ensure_kernel() {
        Some(k) => {
            eprintln!("Using kernel: {}", k.display());
            k
        }
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let rootfs = create_test_rootfs(tmp.path(), 64);
    let serial_log = tmp.path().join("console.log");

    // --- Start with ready-fd ---

    eprintln!("Starting VM with ready-fd pipe...");
    let (mut child, pid, read_file) = spawn_ember_vz(&ember_vz, &kernel, &rootfs, &serial_log);

    // Read MAC from ready-fd (same as MacosVm::start does).
    let mac = match read_mac_from_pipe(read_file, BOOT_TIMEOUT) {
        Some(m) => m,
        None => {
            eprintln!("Failed to read MAC from ready-fd — VM may have crashed");
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            let _ = wait_for_exit(&mut child, STOP_TIMEOUT);
            panic!("VM failed to boot (no MAC on ready-fd)");
        }
    };

    eprintln!("VM booted, MAC: {mac}");

    // Verify MAC format.
    assert!(
        mac.contains(':') && mac.len() >= 11,
        "invalid MAC address: '{mac}'"
    );

    // Verify process is running (same as MacosVm::is_running).
    assert!(is_running(pid), "ember-vz should be running after boot");

    // Let the VM run for a bit.
    std::thread::sleep(Duration::from_secs(2));
    assert!(is_running(pid), "ember-vz should still be running");

    // --- Stop (SIGTERM + wait) ---

    eprintln!("Stopping VM (SIGTERM)...");
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM).expect("SIGTERM failed");
    let status = wait_for_exit(&mut child, STOP_TIMEOUT);
    eprintln!("VM exited: {status}");

    assert!(status.success(), "expected clean exit, got {status}");
    assert!(!is_running(pid), "process should be gone after stop");

    // --- Verify serial output ---

    if serial_log.exists() {
        let serial = std::fs::read_to_string(&serial_log).unwrap_or_default();
        if !serial.is_empty() {
            assert!(
                serial.contains("virtio_blk") || serial.contains("Linux version"),
                "serial log should contain kernel boot messages.\nFirst 300 chars:\n{}",
                &serial[..serial.len().min(300)]
            );
            eprintln!("Serial output verified ({} bytes)", serial.len());
        }
    }

    eprintln!("VM lifecycle test passed: start → ready-fd → is_running → stop");
}

/// Force stop: SIGKILL kills the VM immediately.
#[test]
#[ignore]
fn vm_force_stop() {
    let ember_vz = match ember_vz_bin() {
        Some(p) => p,
        None => {
            eprintln!("Skipping: ember-vz not found");
            return;
        }
    };
    let kernel = match ensure_kernel() {
        Some(k) => k,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let rootfs = create_test_rootfs(tmp.path(), 64);
    let serial_log = tmp.path().join("console.log");

    let (mut child, pid, read_file) = spawn_ember_vz(&ember_vz, &kernel, &rootfs, &serial_log);

    // Wait for boot.
    let _mac = read_mac_from_pipe(read_file, BOOT_TIMEOUT).expect("VM failed to boot");
    assert!(is_running(pid));

    // Force stop (SIGKILL — same as MacosVm::force_stop).
    eprintln!("Sending SIGKILL...");
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL).expect("SIGKILL failed");
    let _ = wait_for_exit(&mut child, Duration::from_secs(5));

    assert!(!is_running(pid), "process should be dead after SIGKILL");
    eprintln!("Force stop test passed");
}

/// Pause (SIGUSR1) and resume (SIGUSR2) keep the process alive.
#[test]
#[ignore]
fn vm_pause_resume() {
    let ember_vz = match ember_vz_bin() {
        Some(p) => p,
        None => {
            eprintln!("Skipping: ember-vz not found");
            return;
        }
    };
    let kernel = match ensure_kernel() {
        Some(k) => k,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let rootfs = create_test_rootfs(tmp.path(), 64);
    let serial_log = tmp.path().join("console.log");

    let (mut child, pid, read_file) = spawn_ember_vz(&ember_vz, &kernel, &rootfs, &serial_log);

    let _mac = read_mac_from_pipe(read_file, BOOT_TIMEOUT).expect("VM failed to boot");

    std::thread::sleep(Duration::from_secs(2));

    // Pause.
    eprintln!("Sending SIGUSR1 (pause)...");
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGUSR1).expect("SIGUSR1 failed");
    std::thread::sleep(Duration::from_secs(1));
    assert!(is_running(pid), "process should be alive while paused");

    // Resume.
    eprintln!("Sending SIGUSR2 (resume)...");
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGUSR2).expect("SIGUSR2 failed");
    std::thread::sleep(Duration::from_secs(1));
    assert!(is_running(pid), "process should be alive after resume");

    // Clean shutdown.
    signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM).expect("SIGTERM failed");
    let _ = wait_for_exit(&mut child, STOP_TIMEOUT);
    assert!(!is_running(pid));

    eprintln!("Pause/resume test passed");
}
