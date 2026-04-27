//! Wrappers around the `thin-provisioning-tools` package: `thin_check`,
//! `thin_repair`, `thin_metadata_size`, `thin_dump`.
//!
//! These are recommended (and in some cases required) for safe pool
//! activation and capacity planning. They live in their own module so
//! the dependency on the `thin-provisioning-tools` package is localized.

use std::path::Path;
use std::process::Command;

use ember_core::error::{Error, Result};

/// Compute a recommended metadata device size in bytes for a pool with
/// `pool_size_bytes` of data, `block_size_bytes` per pool block, and at
/// most `max_thins` concurrent thin volumes.
///
/// Wraps `thin_metadata_size --numeric-only --unit b`. The output is a
/// single integer in bytes.
pub fn metadata_size(
    pool_size_bytes: u64,
    block_size_bytes: u64,
    max_thins: u64,
) -> Result<u64> {
    let output = Command::new("thin_metadata_size")
        .args([
            "--block-size",
            &format!("{block_size_bytes}"),
            "--pool-size",
            &format!("{pool_size_bytes}"),
            "--max-thins",
            &format!("{max_thins}"),
            "--numeric-only",
            "--unit",
            "b",
        ])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "thin_metadata_size".to_string(),
            source: e,
        })?;
    let output = Error::check_command("thin_metadata_size", output)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let bytes = stdout.trim().parse::<u64>().map_err(|e| Error::Command {
        command: "thin_metadata_size".to_string(),
        exit_code: 0,
        stderr: format!("non-numeric output {:?}: {e}", stdout.trim()),
    })?;
    Ok(bytes)
}

/// Run `thin_check` against a metadata device.
///
/// Should be invoked before activating a pool whose metadata may be
/// dirty (e.g., after an unclean shutdown). Returns Ok if the metadata
/// is consistent; otherwise the operator must run [`repair`] manually.
pub fn check(metadata_dev: &Path) -> Result<()> {
    let output = Command::new("thin_check")
        .arg(metadata_dev)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "thin_check".to_string(),
            source: e,
        })?;
    Error::check_command("thin_check", output)?;
    Ok(())
}

/// Repair metadata into a fresh device.
///
/// `thin_repair` reads the (possibly corrupt) input and writes a clean
/// metadata image to `output`. The pool must be offline during repair.
pub fn repair(input: &Path, output: &Path) -> Result<()> {
    let r = Command::new("thin_repair")
        .arg("-i")
        .arg(input)
        .arg("-o")
        .arg(output)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "thin_repair".to_string(),
            source: e,
        })?;
    Error::check_command("thin_repair", r)?;
    Ok(())
}

/// Dump the metadata device's contents as XML.
///
/// Useful for recovery (cross-checking ember's recorded thin ids
/// against what the pool actually holds) and for debug tooling.
/// Returns the raw XML as a string.
pub fn dump(metadata_dev: &Path) -> Result<String> {
    let output = Command::new("thin_dump")
        .arg(metadata_dev)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "thin_dump".to_string(),
            source: e,
        })?;
    let output = Error::check_command("thin_dump", output)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
