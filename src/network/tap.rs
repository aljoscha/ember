//! TAP device creation and cleanup via ioctl.
//!
//! Each Firecracker VM gets a dedicated TAP device (`em-<short-id>`) for
//! its point-to-point network link to the host. This module handles
//! creating and deleting those devices using the Linux TUN/TAP driver.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::process::Command;

use nix::libc;

use crate::error::{Error, Result};

// ── Linux TUN/TAP constants ────────────────────────────────────────
//
// From linux/if_tun.h:
//   TUNSETIFF     = _IOW('T', 202, int)  → 0x400454CA
//   TUNSETPERSIST = _IOW('T', 203, int)  → 0x400454CB
//   IFF_TAP       = 0x0002
//   IFF_NO_PI     = 0x1000
//
// TUNSETIFF is a "bad" ioctl: the kernel header encodes sizeof(int) in
// the ioctl number but the actual argument is a pointer to ifreq. We
// must use the nix `_bad` macro variant with the pre-computed constant.

const TUNSETIFF: libc::c_ulong = 0x4004_54CA;
const TUNSETPERSIST: libc::c_ulong = 0x4004_54CB;
const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;

nix::ioctl_write_ptr_bad!(tunsetiff, TUNSETIFF, libc::ifreq);

/// Create a TAP device with the given name and configure its IP address.
///
/// Opens `/dev/net/tun`, issues `ioctl(TUNSETIFF)` with `IFF_TAP | IFF_NO_PI`,
/// sets the device persistent (so it survives fd close), then assigns an IP
/// address and brings the interface up.
///
/// # Arguments
/// * `name` — device name, e.g. `"em-abc123"` (must be < 16 bytes)
/// * `host_ip` — host-side IP with CIDR prefix, e.g. `"10.100.0.1/30"`
pub fn create(name: &str, host_ip: &str) -> Result<()> {
    if name.len() >= libc::IFNAMSIZ {
        return Err(Error::Network(format!(
            "TAP device name '{name}' too long (max {} bytes)",
            libc::IFNAMSIZ - 1
        )));
    }

    // 1. Open /dev/net/tun.
    let tun_fd = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
        .map_err(|e| Error::Network(format!("failed to open /dev/net/tun: {e}")))?;

    // 2. Prepare ifreq: device name + TAP flags.
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };

    let c_name =
        CString::new(name).map_err(|_| Error::Network("TAP name contains null byte".into()))?;
    let name_bytes = c_name.as_bytes_with_nul();
    unsafe {
        std::ptr::copy_nonoverlapping(
            name_bytes.as_ptr().cast::<libc::c_char>(),
            ifr.ifr_name.as_mut_ptr(),
            name_bytes.len(),
        );
        ifr.ifr_ifru.ifru_flags = IFF_TAP | IFF_NO_PI;
    }

    // 3. Create the TAP device.
    unsafe {
        tunsetiff(tun_fd.as_raw_fd(), &ifr)
            .map_err(|e| Error::Network(format!("ioctl(TUNSETIFF) failed: {e}")))?;
    }

    // 4. Set persistent so the device survives when we close the fd.
    //    Firecracker opens the TAP device by name, so it must outlive this process.
    let ret = unsafe { libc::ioctl(tun_fd.as_raw_fd(), TUNSETPERSIST, 1 as libc::c_int) };
    if ret < 0 {
        let io_err = std::io::Error::last_os_error();
        // Best-effort cleanup.
        let _ = delete(name);
        return Err(Error::Network(format!(
            "ioctl(TUNSETPERSIST) failed: {io_err}"
        )));
    }

    // fd can now be closed — the device persists in the kernel.
    drop(tun_fd);

    // 5. Assign IP address: `ip addr add <host_ip> dev <name>`
    let output = Command::new("ip")
        .args(["addr", "add", host_ip, "dev", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "ip addr add".into(),
            source: e,
        })?;
    if !output.status.success() {
        let _ = delete(name);
        Error::check_command("ip addr add", output)?;
    }

    // 6. Bring the interface up: `ip link set <name> up`
    let output = Command::new("ip")
        .args(["link", "set", name, "up"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "ip link set".into(),
            source: e,
        })?;
    if !output.status.success() {
        let _ = delete(name);
        Error::check_command("ip link set", output)?;
    }

    Ok(())
}

/// Delete a TAP device by name.
///
/// Equivalent to `ip link delete <name>`.
/// Idempotent — returns `Ok(())` if the device does not exist.
pub fn delete(name: &str) -> Result<()> {
    let output = Command::new("ip")
        .args(["link", "delete", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "ip link delete".into(),
            source: e,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // "Cannot find device" means it's already gone — that's fine.
        if stderr.contains("Cannot find device") {
            return Ok(());
        }
        return Err(Error::Network(format!(
            "'ip link delete {name}' failed: {}",
            stderr.trim()
        )));
    }

    Ok(())
}

/// List all ember TAP devices (names starting with `em-`) on the system.
///
/// Parses the output of `ip -o link show type tun` to find persistent
/// TAP devices created by ember. Returns just the device names.
pub fn list_ember_devices() -> Result<Vec<String>> {
    let output = Command::new("ip")
        .args(["-o", "link", "show", "type", "tun"])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "ip link show".into(),
            source: e,
        })?;

    if !output.status.success() {
        // If the command fails (e.g. no tun module loaded), return empty list.
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut devices = Vec::new();
    for line in stdout.lines() {
        // Format: "3: em-abc1234: <...>"
        // Split on ':' and take the second field (device name), trimmed.
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() >= 2 {
            let name = parts[1].trim();
            if name.starts_with("em-") {
                devices.push(name.to_string());
            }
        }
    }

    Ok(devices)
}

/// Check whether a TAP device exists by name.
pub fn exists(name: &str) -> Result<bool> {
    let output = Command::new("ip")
        .args(["link", "show", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "ip link show".into(),
            source: e,
        })?;

    Ok(output.status.success())
}

/// Generate the TAP device name for a VM from its UUID.
///
/// Format: `em-<first 7 hex chars of UUID>`. This fits within the
/// Linux `IFNAMSIZ` limit of 15 characters (3 prefix + 7 hex = 10).
pub fn device_name(vm_id: &uuid::Uuid) -> String {
    let hex = vm_id.as_simple().to_string();
    format!("em-{}", &hex[..7])
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn device_name_format() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let name = device_name(&id);
        assert_eq!(name, "em-550e840");
        assert!(name.len() < libc::IFNAMSIZ);
    }

    #[test]
    fn device_name_fits_ifnamsiz() {
        // Any UUID should produce a name < IFNAMSIZ (16).
        let id = Uuid::new_v4();
        let name = device_name(&id);
        assert!(name.len() < libc::IFNAMSIZ);
    }

    #[test]
    fn name_too_long_is_rejected() {
        let long_name = "a".repeat(libc::IFNAMSIZ);
        let err = create(&long_name, "10.0.0.1/30").unwrap_err();
        assert!(matches!(err, Error::Network(_)));
        let msg = err.to_string();
        assert!(msg.contains("too long"), "unexpected error: {msg}");
    }
}
