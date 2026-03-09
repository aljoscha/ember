//! Integration tests for macOS networking (Phase 4).
//!
//! Tests guest IP discovery from vmnet DHCP leases after booting a VM
//! with `ember-vz`. Verifies that the network backend can find the
//! guest's IP address using the MAC reported via ready-fd.
//!
//! Requirements:
//! - macOS 13+ with Virtualization.framework
//! - `ember-vz` binary built (`swift build` in ember-vz/)
//! - AVF-compatible kernel (see `ensure_kernel()` resolution order)
//! - Homebrew `e2fsprogs` (for mkfs.ext4)
//! - No root required
//!
//! Note: SSH and internet-from-guest tests require a full rootfs with
//! sshd and networking tools (Phase 5 image pipeline). This suite tests
//! the DHCP IP discovery path only.
//!
//! To run:
//!   ./run-integration-tests.sh macos_network
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

/// How long to wait for the guest to obtain a DHCP lease after boot.
const DHCP_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------
// Helpers (shared with macos_vm tests)
// ---------------------------------------------------------------------------

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

fn spawn_ember_vz(
    ember_vz: &Path,
    kernel: &Path,
    rootfs: &Path,
    serial_log: &Path,
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
        "console=hvc0 root=/dev/vda rw ip=dhcp",
        "--serial-log",
        serial_log.to_str().unwrap(),
        "--ready-fd",
        &write_fd_num.to_string(),
    ]);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

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

    drop(write_file);

    let read_file = unsafe { std::fs::File::from_raw_fd(read_raw) };

    (child, pid, read_file)
}

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
// IP Discovery Helpers (mirrors logic in backend::macos::network)
// ---------------------------------------------------------------------------

/// Search /var/db/dhcpd_leases for the given MAC address.
fn find_ip_in_dhcp_leases(mac: &str) -> Option<String> {
    let contents = std::fs::read_to_string("/var/db/dhcpd_leases").ok()?;
    let mac_lower = mac.to_lowercase();

    let mut ip: Option<String> = None;
    let mut hw_mac: Option<String> = None;

    for line in contents.lines() {
        let line = line.trim();
        if line == "{" {
            ip = None;
            hw_mac = None;
        } else if line == "}" {
            if let (Some(ref lease_ip), Some(ref lease_mac)) = (&ip, &hw_mac) {
                if lease_mac == &mac_lower {
                    return Some(lease_ip.clone());
                }
            }
        } else if let Some(value) = line.strip_prefix("ip_address=") {
            ip = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("hw_address=") {
            let mac_part = value.split_once(',').map(|(_, m)| m).unwrap_or(value);
            hw_mac = Some(mac_part.to_lowercase());
        }
    }
    None
}

/// Search `arp -a` output for the given MAC address.
fn find_ip_in_arp(mac: &str) -> Option<String> {
    let output = Command::new("arp").arg("-a").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let normalized_target = normalize_mac(mac);

    for line in stdout.lines() {
        let after_at = line.split(" at ").nth(1)?;
        let arp_mac = after_at.split(" on ").next()?;
        if arp_mac == "(incomplete)" {
            continue;
        }
        if normalize_mac(arp_mac) == normalized_target {
            let start = line.find('(')? + 1;
            let end = line.find(')')?;
            return Some(line[start..end].to_string());
        }
    }
    None
}

/// Normalize MAC: lowercase + zero-pad octets (e.g. "e:49" → "0e:49").
fn normalize_mac(mac: &str) -> String {
    mac.to_lowercase()
        .split(':')
        .map(|o| format!("{:0>2}", o))
        .collect::<Vec<_>>()
        .join(":")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verify that a booted VM gets a DHCP IP from vmnet and we can discover it.
///
/// 1. Boot VM with ember-vz, get MAC from ready-fd
/// 2. Wait for vmnet DHCP to assign an IP
/// 3. Use the network backend's IP discovery (DHCP leases + ARP fallback)
/// 4. Verify the discovered IP is in the vmnet range (192.168.64.x)
#[test]
#[ignore]
fn vm_gets_dhcp_ip() {
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

    // --- Boot VM ---

    eprintln!("Starting VM...");
    let (mut child, pid, read_file) = spawn_ember_vz(&ember_vz, &kernel, &rootfs, &serial_log);

    let mac = match read_mac_from_pipe(read_file, BOOT_TIMEOUT) {
        Some(m) => m,
        None => {
            let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            let _ = wait_for_exit(&mut child, STOP_TIMEOUT);
            panic!("VM failed to boot (no MAC on ready-fd)");
        }
    };
    eprintln!("VM booted, MAC: {mac}");

    // --- Discover guest IP ---

    // Poll DHCP leases and ARP table for the guest's MAC address.
    // The guest needs a moment to complete DHCP negotiation.
    let mut discovered_ip: Option<String> = None;
    let start = Instant::now();
    while start.elapsed() < DHCP_TIMEOUT {
        // Strategy 1: DHCP leases file.
        if let Some(ip) = find_ip_in_dhcp_leases(&mac) {
            discovered_ip = Some(ip);
            break;
        }
        // Strategy 2: ARP table.
        if let Some(ip) = find_ip_in_arp(&mac) {
            discovered_ip = Some(ip);
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    // --- Clean up ---

    eprintln!("Stopping VM...");
    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    let _ = wait_for_exit(&mut child, STOP_TIMEOUT);

    // --- Assertions ---

    let ip = discovered_ip.expect(
        "failed to discover guest IP within timeout — \
         vmnet DHCP may not have assigned a lease",
    );

    eprintln!("Discovered guest IP: {ip}");

    // Verify the IP is in the vmnet shared mode range (192.168.64.0/24).
    assert!(
        ip.starts_with("192.168.64."),
        "expected IP in 192.168.64.0/24 range, got {ip}"
    );

    // Verify it's not the gateway.
    assert_ne!(ip, "192.168.64.1", "guest IP should not be the gateway");

    eprintln!("Network test passed: VM got DHCP IP {ip} via vmnet");
}
