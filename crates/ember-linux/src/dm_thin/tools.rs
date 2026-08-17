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
pub fn metadata_size(pool_size_bytes: u64, block_size_bytes: u64, max_thins: u64) -> Result<u64> {
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

/// Per-volume accounting for one thin device.
#[derive(Debug, PartialEq)]
pub struct ThinRow {
    pub dev_id: u64,
    /// Bytes mapped by this device, blocks shared with an origin
    /// included.
    pub mapped_bytes: u64,
    /// Bytes mapped only by this device. Freed when it is deleted.
    pub exclusive_bytes: u64,
}

/// List per-volume accounting for every thin device in a pool.
///
/// Reads through a reserved metadata snapshot (`-m`), which is the only
/// way to inspect metadata while the kernel owns the live device. The
/// caller must hold a [`pool::MetadataSnap`](super::pool::MetadataSnap)
/// for the duration.
///
/// Reporting through metadata rather than `dmsetup status` also covers
/// volumes that are not currently activated, which is the common case
/// given that ember activates thin devices lazily.
pub fn list_thins(metadata_dev: &Path) -> Result<Vec<ThinRow>> {
    let output = Command::new("thin_ls")
        .args([
            "-m",
            "--no-headers",
            "-o",
            "DEV,MAPPED_BYTES,EXCLUSIVE_BYTES",
        ])
        .arg(metadata_dev)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "thin_ls".to_string(),
            source: e,
        })?;
    let output = Error::check_command("thin_ls", output)?;
    parse_thin_ls(&String::from_utf8_lossy(&output.stdout))
}

fn parse_thin_ls(stdout: &str) -> Result<Vec<ThinRow>> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() != 3 {
                return Err(Error::Command {
                    command: "thin_ls".to_string(),
                    exit_code: 0,
                    stderr: format!("expected 3 fields per row, got {}: {line}", fields.len()),
                });
            }
            Ok(ThinRow {
                dev_id: parse_field(fields[0], "DEV")?,
                mapped_bytes: parse_field(fields[1], "MAPPED_BYTES")?,
                exclusive_bytes: parse_field(fields[2], "EXCLUSIVE_BYTES")?,
            })
        })
        .collect()
}

fn parse_field(s: &str, field: &str) -> Result<u64> {
    s.parse::<u64>().map_err(|e| Error::Command {
        command: "thin_ls".to_string(),
        exit_code: 0,
        stderr: format!("non-numeric {field} value {s:?}: {e}"),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_thin_ls_rows() {
        let out = "  1234    10737418240     8589934592\n\
                   5678     2147483648      1073741824\n";
        let rows = parse_thin_ls(out).unwrap();
        assert_eq!(
            rows,
            vec![
                ThinRow {
                    dev_id: 1234,
                    mapped_bytes: 10_737_418_240,
                    exclusive_bytes: 8_589_934_592,
                },
                ThinRow {
                    dev_id: 5678,
                    mapped_bytes: 2_147_483_648,
                    exclusive_bytes: 1_073_741_824,
                },
            ]
        );
    }

    /// A pool that holds no thin devices yet.
    #[test]
    fn parses_empty_listing() {
        assert_eq!(parse_thin_ls("").unwrap(), vec![]);
        assert_eq!(parse_thin_ls("\n\n").unwrap(), vec![]);
    }

    /// If `--no-headers` ever stops suppressing the header we want a
    /// hard failure, not a row with a garbage device id.
    #[test]
    fn rejects_header_row() {
        assert!(parse_thin_ls("DEV MAPPED_BYTES EXCLUSIVE_BYTES\n").is_err());
    }

    #[test]
    fn rejects_short_row() {
        assert!(parse_thin_ls("1234 10737418240\n").is_err());
    }
}
