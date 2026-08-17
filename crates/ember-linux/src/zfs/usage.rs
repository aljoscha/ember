//! Space accounting queries against ZFS.
//!
//! Both queries use `-p` so ZFS emits exact byte counts rather than the
//! human-readable abbreviations `zfs list` prints by default.

use std::process::Command;

use ember_core::error::{Error, Result};

/// Per-volume accounting for one zvol.
///
/// The occupancy figure is [`used_by_dataset`](Self::used_by_dataset)
/// and deliberately not the `used` property, even though `used` is what
/// a destroy would return to the pool. Two things inflate `used` past
/// what the volume physically holds:
///
/// * A zvol from `zfs create -V` carries a refreservation for its whole
///   virtual size, which `used` counts as consumed. Our image volumes
///   report a `used` of 8.4 GiB against a `referenced` of 1.9 GiB.
/// * `usedbysnapshots` is by definition space the live volume no longer
///   references, so adding it would push occupancy past `referenced`.
///
/// Clones carry no reservation and ember's fork snapshots hold almost
/// nothing, so in practice the two agree for VMs and diverge for images.
#[derive(Debug, PartialEq)]
pub struct VolumeRow {
    /// Full dataset path, e.g. `tank/ember/vms/myvm`.
    pub name: String,
    pub volsize: u64,
    /// Blocks referenced by the live volume and by nothing else. A
    /// subset of `referenced` by ZFS's own definition.
    pub used_by_dataset: u64,
    /// Reserved but unwritten space, charged to the pool by
    /// `refreservation`. Zero for clones.
    pub used_by_refreservation: u64,
    /// Addressable space including blocks shared with an origin.
    pub referenced: u64,
    /// Uncompressed size of `referenced`.
    pub logical_referenced: u64,
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
            "name,volsize,usedbydataset,usedbyrefreservation,referenced,logicalreferenced",
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
                used_by_refreservation: super::parse_u64(fields[3], "usedbyrefreservation")?,
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

    /// Rows captured verbatim from a live pool: a clone (no
    /// reservation) and an image volume (reserved by `zfs create -V`).
    #[test]
    fn parses_volume_rows() {
        let out =
            "ember/ember/vms/aj-dev\t214748364800\t104418334720\t0\t105790773760\t208040643072\n\
             ember/ember/images/ubuntu-dev\t6810501120\t2079834112\t6919027712\t2079834112\t4660178944\n";
        let rows = parse_volumes(out).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "ember/ember/vms/aj-dev");
        assert_eq!(rows[0].volsize, 214_748_364_800);
        assert_eq!(rows[0].used_by_dataset, 104_418_334_720);
        assert_eq!(rows[0].used_by_refreservation, 0);
        assert_eq!(rows[0].referenced, 105_790_773_760);
        assert_eq!(rows[0].logical_referenced, 208_040_643_072);
        assert_eq!(rows[1].used_by_refreservation, 6_919_027_712);
    }

    /// Regression, on the exact rows the live pool produces. An earlier
    /// cut summed `usedbydataset + usedbysnapshots` for occupancy, and
    /// since snapshot-only space is by definition outside `referenced`,
    /// both image rows shipped with exclusive above referenced.
    #[test]
    fn occupancy_never_exceeds_referenced() {
        let out = "\
ember/ember/vms/aj-dev\t214748364800\t104418334720\t0\t105790773760\t208040643072
ember/ember/vms/mz-dev\t214748364800\t8803586560\t0\t10869327872\t22587842560
ember/ember/images/ubuntu-dev\t6810501120\t2079834112\t6919027712\t2079834112\t4660178944
ember/ember/images/ubuntu-dev-new\t7147094016\t2158472704\t7260863488\t2158472704\t4885119488
";
        for row in parse_volumes(out).unwrap() {
            assert!(
                row.used_by_dataset <= row.referenced,
                "{}: occupancy {} exceeds referenced {}",
                row.name,
                row.used_by_dataset,
                row.referenced
            );
        }
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
