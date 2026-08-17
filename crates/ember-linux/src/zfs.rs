pub mod dataset;
pub mod pool;
pub mod snapshot;
pub mod usage;
pub mod volume;

use std::process::Command;

use ember_core::error::{Error, Result};

/// The reserved snapshot name used for image cloning.
///
/// Every image zvol has a `@base` snapshot that serves as the clone source
/// for per-VM zvols. This name is checked in snapshot create/delete commands
/// and filtered from user-facing snapshot listings.
pub const BASE_SNAPSHOT_NAME: &str = "base";

/// Run `zfs destroy` on a dataset, volume, or snapshot.
///
/// With `recursive: true`, passes `-r` to also destroy children and snapshots.
/// With `force_dependents: true`, passes `-R` to also destroy dependent clones
/// (e.g., VM zvols cloned from an image snapshot). `-R` implies `-r`.
pub(crate) fn destroy(name: &str, recursive: bool) -> Result<()> {
    destroy_impl(name, recursive, false)
}

/// Like [`destroy`], but with `-R` to also destroy dependent clones.
pub(crate) fn destroy_with_dependents(name: &str) -> Result<()> {
    destroy_impl(name, false, true)
}

fn destroy_impl(name: &str, recursive: bool, force_dependents: bool) -> Result<()> {
    let mut args = vec!["destroy"];
    if force_dependents {
        args.push("-R");
    } else if recursive {
        args.push("-r");
    }
    args.push(name);

    let output = Command::new("zfs")
        .args(&args)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs destroy".to_string(),
            source: e,
        })?;

    Error::check_command("zfs destroy", output)?;
    Ok(())
}

/// Parse a numeric string from ZFS tab-separated output into a `u64`.
///
/// Used across all ZFS modules to parse byte counts, timestamps, and
/// other numeric fields from `zfs list` / `zpool list` output.
pub(crate) fn parse_u64(s: &str, field: &str) -> Result<u64> {
    s.trim()
        .parse::<u64>()
        .map_err(|_| Error::Zfs(format!("cannot parse {field} value: {s}")))
}
