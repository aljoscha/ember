//! Generic device-mapper plumbing shared by every dm target we drive.
//!
//! Everything here is target-agnostic: creating, removing, suspending,
//! and messaging a device works the same whether it is a `thin-pool`, a
//! `thin` volume, or a `vdo`. Target-specific knowledge (table layout,
//! status fields, naming) belongs in the module that owns the target,
//! which is why this one only ever handles opaque table strings.

use std::path::PathBuf;
use std::process::Command;

use ember_core::error::{Error, Result};

/// Sectors are always 512 bytes on Linux block devices.
pub const SECTOR_SIZE: u64 = 512;

/// Path a named device-mapper device is exposed at once activated.
pub fn device_path(name: &str) -> PathBuf {
    PathBuf::from(format!("/dev/mapper/{name}"))
}

/// Whether a device-mapper device with the given name is currently
/// active.
pub fn device_exists(name: &str) -> Result<bool> {
    let output = Command::new("dmsetup")
        .args(["info", "--noheadings", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup info".to_string(),
            source: e,
        })?;
    Ok(output.status.success())
}

/// Activate a device from a complete table line.
pub fn create(name: &str, table: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["create", name, "--table", table])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup create".to_string(),
            source: e,
        })?;
    Error::check_command(&format!("dmsetup create {name}"), output)?;
    Ok(())
}

/// Tear down a device. Backing storage and any on-disk metadata are
/// untouched, so the device can be re-created later.
pub fn remove(name: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["remove", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup remove".to_string(),
            source: e,
        })?;
    Error::check_command(&format!("dmsetup remove {name}"), output)?;
    Ok(())
}

/// Suspend I/O to a device. Required before loading a new table.
pub fn suspend(name: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["suspend", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup suspend".to_string(),
            source: e,
        })?;
    Error::check_command(&format!("dmsetup suspend {name}"), output)?;
    Ok(())
}

/// Resume a previously suspended device.
pub fn resume(name: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["resume", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup resume".to_string(),
            source: e,
        })?;
    Error::check_command(&format!("dmsetup resume {name}"), output)?;
    Ok(())
}

/// Swap in a new table on a live device.
///
/// The kernel requires suspend, load, resume in that order. If the load
/// fails we resume anyway before returning the error, so a rejected
/// table leaves the device running on its old one rather than
/// suspended and wedged.
pub fn reload(name: &str, table: &str) -> Result<()> {
    suspend(name)?;
    if let Err(e) = load(name, table) {
        // Best-effort resume so a rejected table leaves the device
        // running on its old one rather than suspended and wedged.
        let _ = resume(name);
        return Err(e);
    }
    resume(name)
}

/// Stage a table in the device's inactive slot and swap it in without
/// suspending.
///
/// `resume` promotes the inactive table whether or not the device was
/// suspended, so this is a complete swap. Some targets ask for it: the
/// vdo documentation states a modified table may be loaded into a
/// running, non-suspended volume, and suspending one with a live target
/// stacked above it risks blocking on in-flight I/O that the upper
/// layer is still issuing.
pub fn swap_table(name: &str, table: &str) -> Result<()> {
    load(name, table)?;
    resume(name)
}

/// Stage a table in the device's inactive slot. Takes effect on the
/// next [`resume`].
fn load(name: &str, table: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["load", name, "--table", table])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup load".to_string(),
            source: e,
        })?;
    Error::check_command(&format!("dmsetup load {name}"), output)?;
    Ok(())
}

/// Rename an active device.
///
/// Atomic from the kernel's perspective: I/O in flight against the
/// device keeps working across the rename.
pub fn rename(old_name: &str, new_name: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["rename", old_name, new_name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup rename".to_string(),
            source: e,
        })?;
    Error::check_command(&format!("dmsetup rename {old_name}"), output)?;
    Ok(())
}

/// Send a control message to a device.
///
/// Several targets deliver most of their operations this way rather
/// than through dedicated `dmsetup` subcommands.
pub fn message(name: &str, msg: &str) -> Result<()> {
    let output = Command::new("dmsetup")
        .args(["message", name, "0", msg])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup message".to_string(),
            source: e,
        })?;
    Error::check_command(&format!("dmsetup message {name} {msg}"), output)?;
    Ok(())
}

/// Send a control message and return what the target wrote back.
///
/// Targets that report rather than act put their answer in the message
/// response buffer, which `dmsetup` prints on stdout.
pub fn message_response(name: &str, msg: &str) -> Result<String> {
    let output = Command::new("dmsetup")
        .args(["message", name, "0", msg])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup message".to_string(),
            source: e,
        })?;
    let output = Error::check_command(&format!("dmsetup message {name} {msg}"), output)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Raw `dmsetup status` line for a device. Parsing the target-specific
/// tail is the caller's job.
pub fn status(name: &str) -> Result<String> {
    let output = Command::new("dmsetup")
        .args(["status", name])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup status".to_string(),
            source: e,
        })?;
    let output = Error::check_command(&format!("dmsetup status {name}"), output)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Active device names starting with `prefix`. Used to sweep up an
/// installation's volumes during teardown.
pub fn list_with_prefix(prefix: &str) -> Result<Vec<String>> {
    let output = Command::new("dmsetup")
        .arg("ls")
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup ls".to_string(),
            source: e,
        })?;
    let output = Error::check_command("dmsetup ls", output)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            if name.starts_with(prefix) {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect())
}

/// Ensure the kernel provides a device-mapper target.
///
/// On most distributions these ship as loadable modules that `dmsetup`
/// does not auto-load. We `modprobe` best-effort (a built-in target
/// reports "Module not found" but is already registered) and then
/// confirm it shows up in `dmsetup targets`. Without the check, a
/// missing target surfaces as an opaque `Invalid argument` from
/// `dmsetup create`.
///
/// `config_hint` names the kernel config symbol to suggest when the
/// target is genuinely absent.
pub fn ensure_target(module: &str, target: &str, config_hint: &str) -> Result<()> {
    let _ = Command::new("modprobe").arg(module).output();

    let output = Command::new("dmsetup")
        .arg("targets")
        .output()
        .map_err(|e| Error::CommandExec {
            command: "dmsetup targets".to_string(),
            source: e,
        })?;
    let output = Error::check_command("dmsetup targets", output)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if has_target(&stdout, target) {
        return Ok(());
    }
    Err(Error::Command {
        command: "dmsetup targets".to_string(),
        exit_code: 0,
        stderr: format!(
            "kernel does not provide the '{target}' device-mapper target. \
             Install or enable a kernel with {config_hint} and load it with \
             'modprobe {module}'."
        ),
    })
}

/// Whether `dmsetup targets` output advertises `target`. The listing is
/// `<name> v<major>.<minor>.<patch>` per line, so we match the first
/// field exactly rather than substring-matching the whole blob, which
/// would let `thin` match `thin-pool`.
fn has_target(listing: &str, target: &str) -> bool {
    listing
        .lines()
        .any(|line| line.split_whitespace().next() == Some(target))
}

/// Whether an [`Error`] reports a kernel `EEXIST` from a `dmsetup`
/// operation.
///
/// Callers that allocate an identifier and retry on collision depend on
/// telling this apart from a real failure.
///
/// `dmsetup` translates the kernel's `-EEXIST` into a stderr line that
/// embeds the libc `strerror` for `EEXIST` — `"File exists"` on glibc
/// and musl. The exact wrapping line has shifted across `lvm2`
/// releases (e.g. `"device-mapper: message ioctl on ember-pool failed:
/// File exists"`), but the trailing strerror is stable. Pinned and
/// regression-tested against:
///
/// * Linux 6.1+ (Debian 12, Ubuntu 24.04)
/// * `lvm2` 2.03.x (Debian / Fedora packaging from 2023+)
/// * glibc and musl (`strerror(EEXIST) == "File exists"`)
///
/// If a future kernel/util-linux/libc combination changes the wording,
/// retries will turn into hard failures rather than collide silently —
/// the [`tests::matches_dmsetup_eexist_message`] test below would be
/// the first thing to fail.
pub fn is_already_exists(err: &Error) -> bool {
    matches!(err, Error::Command { stderr, .. } if stderr.contains("File exists"))
}

/// Whether an [`Error`] reports a kernel `EBUSY`, meaning the resource
/// a message asked for is already held.
///
/// Same stability argument as [`is_already_exists`]: `dmsetup` embeds
/// the libc strerror, and `strerror(EBUSY)` is `"Device or resource
/// busy"` on glibc and musl.
pub fn is_busy(err: &Error) -> bool {
    matches!(err, Error::Command { stderr, .. } if stderr.contains("Device or resource busy"))
}

/// Whether an [`Error`] reports a kernel `EINVAL`.
///
/// Targets reject a table they consider malformed with `-EINVAL`, which
/// covers a wide range of causes. Callers use this only to decide
/// whether to replace the kernel's bare message with a domain-specific
/// explanation of the most likely cause, so a false positive costs a
/// misleading hint rather than a wrong action.
pub fn is_invalid_argument(err: &Error) -> bool {
    matches!(err, Error::Command { stderr, .. } if stderr.contains("Invalid argument"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: mirror an actual `dmsetup message` failure line so a
    /// future glibc/lvm2 wording change is loud, not silent. Captured
    /// from a Linux 6.1 / lvm2 2.03 host attempting `create_thin` with
    /// a duplicate id.
    #[test]
    fn matches_dmsetup_eexist_message() {
        let err = Error::Command {
            command: "dmsetup".to_string(),
            exit_code: 1,
            stderr: "device-mapper: message ioctl on ember-pool failed: File exists\n".to_string(),
        };
        assert!(is_already_exists(&err));
    }

    #[test]
    fn rejects_unrelated_errors() {
        let err = Error::Command {
            command: "dmsetup".to_string(),
            exit_code: 1,
            stderr: "device-mapper: reload ioctl on ember-pool failed: Invalid argument\n"
                .to_string(),
        };
        assert!(!is_already_exists(&err));
        assert!(is_invalid_argument(&err));
    }

    #[test]
    fn rejects_non_command_errors() {
        let err = Error::Vm("File exists somewhere else in the system".to_string());
        assert!(!is_already_exists(&err));
        assert!(!is_busy(&err));
        assert!(!is_invalid_argument(&err));
    }

    /// Companion to [`matches_dmsetup_eexist_message`], pinning the
    /// wording `reserve_metadata_snap` fails with when a snapshot is
    /// already held.
    #[test]
    fn matches_dmsetup_ebusy_message() {
        let err = Error::Command {
            command: "dmsetup".to_string(),
            exit_code: 1,
            stderr: "device-mapper: message ioctl on ember-pool failed: Device or resource busy\n"
                .to_string(),
        };
        assert!(is_busy(&err));
        assert!(!is_already_exists(&err));
    }

    /// `thin` is a prefix of `thin-pool`, so a substring match against
    /// the whole listing would claim `thin-pool` is present on a kernel
    /// that only has `thin`.
    #[test]
    fn target_match_is_exact_not_substring() {
        let listing = "thin             v1.23.0\nstriped          v1.6.0\n";
        assert!(has_target(listing, "thin"));
        assert!(!has_target(listing, "thin-pool"));
        assert!(!has_target(listing, "vdo"));
    }

    #[test]
    fn finds_target_anywhere_in_listing() {
        let listing = "striped          v1.6.0\nvdo              v9.1.0\nlinear v1.4.0\n";
        assert!(has_target(listing, "vdo"));
    }
}
