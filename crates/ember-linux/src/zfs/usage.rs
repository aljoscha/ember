//! Space accounting queries against ZFS.
//!
//! Both queries use `-p` so ZFS emits exact byte counts rather than the
//! human-readable abbreviations `zfs list` prints by default.

use std::process::Command;

use ember_core::error::{Error, Result};

/// Per-volume accounting for one zvol.
#[derive(Debug, PartialEq)]
pub struct VolumeRow {
    /// Full dataset path, e.g. `tank/ember/vms/myvm`.
    pub name: String,
    pub volsize: u64,
    /// Blocks held by the live volume, excluding anything shared with
    /// an origin snapshot.
    pub used_by_dataset: u64,
    /// Blocks held only by this volume's own snapshots.
    pub used_by_snapshots: u64,
    /// Addressable space including blocks shared with an origin.
    pub referenced: u64,
    /// Uncompressed size of `referenced`.
    pub logical_referenced: u64,
}

impl VolumeRow {
    /// Blocks that belong to this volume alone.
    ///
    /// Deliberately not the `used` property. A zvol created with `zfs
    /// create -V` carries a refreservation for its whole virtual size,
    /// and `used` counts that reservation as consumed space. That makes
    /// an image report more exclusive bytes than it references, which
    /// is nonsense as an occupancy figure. Clones have no reservation,
    /// so this only ever differs for image volumes.
    pub fn exclusive(&self) -> u64 {
        self.used_by_dataset.saturating_add(self.used_by_snapshots)
    }
}

/// Dataset-tree totals, used for pool-level reporting.
#[derive(Debug, PartialEq)]
pub struct DatasetTotals {
    pub used: u64,
    pub available: u64,
    pub logical_used: u64,
}

/// Accounting for every zvol under `base`, recursively.
///
/// One call covers both the `images/` and `vms/` subtrees.
pub fn volumes(base: &str) -> Result<Vec<VolumeRow>> {
    let output = Command::new("zfs")
        .args([
            "list",
            "-Hp",
            "-r",
            "-t",
            "volume",
            "-o",
            "name,volsize,usedbydataset,usedbysnapshots,referenced,logicalreferenced",
            base,
        ])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs list".to_string(),
            source: e,
        })?;
    let output = Error::check_command("zfs list volumes", output)?;
    parse_volumes(&String::from_utf8_lossy(&output.stdout))
}

fn parse_volumes(stdout: &str) -> Result<Vec<VolumeRow>> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() != 6 {
                return Err(Error::Zfs(format!(
                    "expected 6 tab-separated fields from `zfs list`, got {}: {line}",
                    fields.len()
                )));
            }
            Ok(VolumeRow {
                name: fields[0].to_string(),
                volsize: super::parse_u64(fields[1], "volsize")?,
                used_by_dataset: super::parse_u64(fields[2], "usedbydataset")?,
                used_by_snapshots: super::parse_u64(fields[3], "usedbysnapshots")?,
                referenced: super::parse_u64(fields[4], "referenced")?,
                logical_referenced: super::parse_u64(fields[5], "logicalreferenced")?,
            })
        })
        .collect()
}

/// Totals for the dataset tree ember owns.
///
/// We report against the dataset rather than the raw vdev (`zpool
/// list`) so that per-volume numbers sum into the pool figure, and so
/// quotas and sibling datasets on a shared pool are accounted for.
pub fn totals(base: &str) -> Result<DatasetTotals> {
    let output = Command::new("zfs")
        .args([
            "get",
            "-Hp",
            "-o",
            "value",
            "used,available,logicalused",
            base,
        ])
        .output()
        .map_err(|e| Error::CommandExec {
            command: "zfs get".to_string(),
            source: e,
        })?;
    let output = Error::check_command("zfs get totals", output)?;
    parse_totals(&String::from_utf8_lossy(&output.stdout))
}

/// `zfs get` emits one line per property, in the order requested.
fn parse_totals(stdout: &str) -> Result<DatasetTotals> {
    let values: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if values.len() != 3 {
        return Err(Error::Zfs(format!(
            "expected 3 property values from `zfs get`, got {}",
            values.len()
        )));
    }
    Ok(DatasetTotals {
        used: super::parse_u64(values[0], "used")?,
        available: super::parse_u64(values[1], "available")?,
        logical_used: super::parse_u64(values[2], "logicalused")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a live pool: a clone (no reservation) and an
    /// image volume (reserved by `zfs create -V`).
    #[test]
    fn parses_volume_rows() {
        let out =
            "ember/ember/vms/aj-dev\t214748364800\t104418334720\t0\t105790773760\t208040643072\n\
             ember/ember/images/ubuntu-dev\t6810501120\t2079834112\t1024\t2079834112\t4660178944\n";
        let rows = parse_volumes(out).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "ember/ember/vms/aj-dev");
        assert_eq!(rows[0].volsize, 214_748_364_800);
        assert_eq!(rows[0].exclusive(), 104_418_334_720);
        assert_eq!(rows[0].referenced, 105_790_773_760);
        assert_eq!(rows[0].logical_referenced, 208_040_643_072);
        assert_eq!(rows[1].exclusive(), 2_079_834_112 + 1024);
    }

    /// Regression: an image zvol's `used` includes its refreservation,
    /// which would put exclusive above referenced. Exclusive has to
    /// stay within referenced for a volume with no snapshots.
    #[test]
    fn reserved_volume_does_not_exceed_referenced() {
        // volsize, usedbydataset, usedbysnapshots, referenced, logicalreferenced
        let out = "p/images/x\t6810501120\t2079834112\t0\t2079834112\t4660178944\n";
        let row = &parse_volumes(out).unwrap()[0];
        assert!(row.exclusive() <= row.referenced);
    }

    /// A pool with no zvols yet is not an error.
    #[test]
    fn parses_empty_listing() {
        assert_eq!(parse_volumes("").unwrap(), vec![]);
        assert_eq!(parse_volumes("\n").unwrap(), vec![]);
    }

    /// ZFS emits `-` for properties that do not apply. We would rather
    /// fail loudly than silently record a zero-sized volume.
    #[test]
    fn rejects_non_numeric_field() {
        let out = "ember/ember/vms/x\t100\t-\t0\t100\t100\n";
        assert!(parse_volumes(out).is_err());
    }

    #[test]
    fn rejects_short_row() {
        assert!(parse_volumes("ember/ember/vms/x\t100\t100\n").is_err());
    }

    #[test]
    fn parses_totals() {
        let out = "320637513728\n196273849856\n643565009920\n";
        assert_eq!(
            parse_totals(out).unwrap(),
            DatasetTotals {
                used: 320_637_513_728,
                available: 196_273_849_856,
                logical_used: 643_565_009_920,
            }
        );
    }

    #[test]
    fn rejects_truncated_totals() {
        assert!(parse_totals("320637513728\n196273849856\n").is_err());
    }
}
