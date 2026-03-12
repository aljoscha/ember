//! Shared test helpers for macOS integration tests.
//!
//! Provides common utilities for locating binaries, creating test images,
//! spawning ember-vz, and reading from ready-fd pipes.
//!
//! Not all test files use all helpers — dead_code warnings are suppressed
//! via `#[allow(dead_code)]` on the `mod common;` declaration in each test file.

use std::io::{BufRead, BufReader};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Ember CLI helpers
// ---------------------------------------------------------------------------

/// Path to the ember binary built by cargo.
pub fn ember_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("ember");
    path
}

/// Run ember with the given args, returning the Output.
pub fn ember(args: &[&str]) -> std::process::Output {
    Command::new(ember_bin())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute ember: {e}"))
}

/// Run `ember init` with a temporary state directory, returning the state dir path.
pub fn setup_init(tmp: &Path) -> PathBuf {
    let state_dir = tmp.join("state");
    let output = ember(&["--state-dir", state_dir.to_str().unwrap(), "init"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "init failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    state_dir
}

// ---------------------------------------------------------------------------
// ember-vz helpers
// ---------------------------------------------------------------------------

/// Locate the `ember-vz` Swift helper binary.
///
/// Resolution order:
/// 1. `EMBER_VZ` env var
/// 2. `ember-vz/.build/release/ember-vz` (relative to project root)
/// 3. `ember-vz/.build/debug/ember-vz`
pub fn ember_vz_bin() -> Option<PathBuf> {
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

/// Get a bootable kernel for AVF tests.
///
/// Resolution order:
/// 1. `EMBER_TEST_KERNEL` env var (explicit override)
/// 2. Cached at `/tmp/ember-test-vmlinux`
/// 3. Local build at `kernel/vmlinux`
///
/// Returns `None` if no kernel is available (test should skip gracefully).
pub fn ensure_kernel() -> Option<PathBuf> {
    const KERNEL_CACHE_PATH: &str = "/tmp/ember-test-vmlinux";

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
        eprintln!("Using locally built kernel: {}", local_kernel.display());
        let _ = std::fs::copy(&local_kernel, &cache);
        return Some(cache);
    }

    eprintln!(
        "Skipping: no AVF-compatible kernel found.\n\
         Build one with: cd kernel && make docker-build ARCH=arm64 FRAGMENTS=\"avf.fragment\""
    );
    None
}

// ---------------------------------------------------------------------------
// e2fsprogs / rootfs helpers
// ---------------------------------------------------------------------------

/// Find an e2fsprogs tool by checking Homebrew paths before falling back to PATH.
pub fn find_e2fsprogs_tool(name: &str) -> String {
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

/// Create a minimal ext4 rootfs image file.
pub fn create_test_rootfs(dir: &Path, size_mb: u64) -> PathBuf {
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

// ---------------------------------------------------------------------------
// Process / pipe helpers
// ---------------------------------------------------------------------------

/// Spawn ember-vz with a ready-fd pipe.
///
/// Returns (child, pid, read_file) where read_file is the parent's end of
/// the pipe that ember-vz writes the MAC address to.
pub fn spawn_ember_vz(
    ember_vz: &Path,
    kernel: &Path,
    rootfs: &Path,
    serial_log: &Path,
    boot_args: &str,
) -> (std::process::Child, u32, std::fs::File) {
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
        boot_args,
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
pub fn read_mac_from_pipe(read_file: std::fs::File, timeout: Duration) -> Option<String> {
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
pub fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> std::process::ExitStatus {
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
