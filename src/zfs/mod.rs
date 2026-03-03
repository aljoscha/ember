pub mod dataset;
pub mod pool;
pub mod snapshot;
pub mod volume;

use crate::error::{Error, Result};

/// Parse a numeric string from ZFS tab-separated output into a `u64`.
///
/// Used across all ZFS modules to parse byte counts, timestamps, and
/// other numeric fields from `zfs list` / `zpool list` output.
pub(crate) fn parse_u64(s: &str, field: &str) -> Result<u64> {
    s.trim()
        .parse::<u64>()
        .map_err(|_| Error::Zfs(format!("cannot parse {field} value: {s}")))
}
