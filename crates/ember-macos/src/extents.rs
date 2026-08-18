//! Physical extent maps, and what a set of files really occupies.
//!
//! APFS clones share physical blocks, and `st_blocks` cannot see it: it
//! counts the blocks a file maps, not the ones it owns, so a fresh clone
//! reports its origin's full figure while costing nothing. The sharing
//! is visible one level down. `fcntl(F_LOG2PHYS_EXT)` maps a logical
//! offset to the physical byte range backing it, so two files that map
//! the same physical bytes are demonstrably sharing them.
//!
//! [`scan`] reads one file's extents. [`occupancy`] sweeps several
//! files' extents together and splits them into what each file holds
//! alone and what the whole set occupies. The split only means anything
//! across a whole set, which is why the caller hands us every volume at
//! once rather than asking per file.

use std::os::unix::io::AsRawFd;
use std::path::Path;

use ember_core::error::{Error, Result};

/// A contiguous run of physical bytes on the volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Extent {
    /// Byte offset on the underlying device.
    pub start: u64,
    pub len: u64,
}

/// What a set of files occupies once shared blocks are counted once.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Occupancy {
    /// Bytes mapped by exactly one file, indexed as the input was.
    pub exclusive: Vec<u64>,
    /// Bytes mapped by at least one file, each counted a single time.
    /// This is what the set actually costs on disk.
    pub union: u64,
}

/// Physical extents of `path`, or `None` if it no longer exists.
///
/// Holes are skipped with `SEEK_DATA` rather than probed block by
/// block, which matters because a VM rootfs is mostly hole: an 8 GiB
/// image holding a 300 MiB filesystem would otherwise cost two million
/// syscalls to walk.
pub(crate) fn scan(path: &Path) -> Result<Option<Vec<Extent>>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source: e,
            })
        }
    };
    let size = file
        .metadata()
        .map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?
        .len();

    let fd = file.as_raw_fd();
    let mut extents = Vec::new();
    let mut offset = 0u64;

    while offset < size {
        // SEEK_DATA lands on the next byte that is actually backed.
        // ENXIO means there is none left, which is the normal way out
        // of a file that ends in a hole.
        let data = unsafe { nix::libc::lseek(fd, offset as i64, nix::libc::SEEK_DATA) };
        if data < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(nix::libc::ENXIO) {
                break;
            }
            return Err(Error::Io {
                path: path.to_path_buf(),
                source: err,
            });
        }
        let data = data as u64;
        if data >= size {
            break;
        }

        // On the way in, `l2p_devoffset` is the logical offset we are
        // asking about and `l2p_contigbytes` is how much we would like
        // covered. On the way out both describe the physical side.
        let mut l2p: nix::libc::log2phys = unsafe { std::mem::zeroed() };
        l2p.l2p_devoffset = data as i64;
        l2p.l2p_contigbytes = (size - data) as i64;
        let rc = unsafe { nix::libc::fcntl(fd, nix::libc::F_LOG2PHYS_EXT, &mut l2p) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            // ERANGE means the range is not mappable, which we treat as
            // the end of what we can account for rather than an error.
            // SEEK_DATA promised data here, so this is not expected.
            if err.raw_os_error() == Some(nix::libc::ERANGE) {
                break;
            }
            return Err(Error::Io {
                path: path.to_path_buf(),
                source: err,
            });
        }

        // A non-positive length would leave `offset` where it is and
        // spin forever, so it ends the walk instead.
        if l2p.l2p_contigbytes <= 0 {
            break;
        }
        let len = l2p.l2p_contigbytes as u64;
        extents.push(Extent {
            start: l2p.l2p_devoffset as u64,
            len,
        });
        offset = data.saturating_add(len);
    }

    Ok(Some(extents))
}

/// Split the physical bytes of several files into per-file exclusive
/// holdings and the union across all of them.
///
/// A byte mapped by exactly one file is exclusive to it. A byte mapped
/// by several belongs to none of them exclusively, which is what makes
/// an image's `exclusive` fall to nothing while its clones live.
///
/// Pure interval arithmetic, so it is testable without APFS underneath.
pub(crate) fn occupancy(files: &[Vec<Extent>]) -> Occupancy {
    // Sweep boundaries left to right, tracking how many files cover the
    // segment we are standing on.
    let mut events: Vec<(u64, i8, usize)> = Vec::new();
    for (idx, extents) in files.iter().enumerate() {
        for e in extents {
            if e.len == 0 {
                continue;
            }
            events.push((e.start, 1, idx));
            events.push((e.start.saturating_add(e.len), -1, idx));
        }
    }
    events.sort_unstable();

    let mut depth = vec![0i32; files.len()];
    let mut exclusive = vec![0u64; files.len()];
    let mut union = 0u64;
    let mut covering = 0usize;
    // While exactly one file covers the segment, the sum of the indices
    // of the covering files is that file's index. Cheaper than scanning
    // `depth` for the survivor at every boundary.
    let mut index_sum = 0usize;

    let mut i = 0;
    while i < events.len() {
        let pos = events[i].0;
        while i < events.len() && events[i].0 == pos {
            let (_, delta, idx) = events[i];
            if delta > 0 {
                depth[idx] += 1;
                if depth[idx] == 1 {
                    covering += 1;
                    index_sum += idx;
                }
            } else {
                depth[idx] -= 1;
                if depth[idx] == 0 {
                    covering -= 1;
                    index_sum -= idx;
                }
            }
            i += 1;
        }
        if i < events.len() && covering > 0 {
            let seg = events[i].0 - pos;
            union += seg;
            if covering == 1 {
                exclusive[index_sum] += seg;
            }
        }
    }

    Occupancy { exclusive, union }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(start: u64, len: u64) -> Extent {
        Extent { start, len }
    }

    /// Pristine clones map the same bytes, so none of them holds
    /// anything alone and the set costs one copy.
    #[test]
    fn pristine_clones_hold_nothing_exclusively() {
        let shared = vec![ext(0, 1000)];
        let got = occupancy(&[shared.clone(), shared.clone(), shared]);
        assert_eq!(got.exclusive, vec![0, 0, 0]);
        assert_eq!(got.union, 1000);
    }

    /// A clone that rewrote part of itself holds exactly what it
    /// rewrote, and the origin keeps the part still shared.
    #[test]
    fn a_diverged_clone_holds_exactly_what_it_rewrote() {
        let origin = vec![ext(0, 1000)];
        // First 400 bytes rewritten elsewhere, the rest still shared.
        let clone = vec![ext(5000, 400), ext(400, 600)];
        let got = occupancy(&[origin, clone]);
        assert_eq!(got.exclusive, vec![400, 400]);
        assert_eq!(got.union, 1400);
    }

    /// Sharing is not pairwise. A byte held by three files is exclusive
    /// to none of them.
    #[test]
    fn a_byte_shared_three_ways_is_exclusive_to_none() {
        let got = occupancy(&[
            vec![ext(0, 100)],
            vec![ext(0, 100)],
            vec![ext(0, 100), ext(100, 50)],
        ]);
        assert_eq!(got.exclusive, vec![0, 0, 50]);
        assert_eq!(got.union, 150);
    }

    /// The union counts a shared byte once, which is the whole reason
    /// the pool figure cannot be a sum of the per-volume numbers.
    #[test]
    fn union_counts_a_shared_byte_once() {
        let got = occupancy(&[vec![ext(0, 800)], vec![ext(0, 800)], vec![ext(0, 800)]]);
        let sum_of_referenced: u64 = 800 * 3;
        assert_eq!(got.union, 800);
        assert!(got.union < sum_of_referenced);
    }

    /// Extents that touch but do not overlap stay exclusive, and the
    /// sweep must not merge them into a shared region at the seam.
    #[test]
    fn adjacent_extents_do_not_count_as_shared() {
        let got = occupancy(&[vec![ext(0, 100)], vec![ext(100, 100)]]);
        assert_eq!(got.exclusive, vec![100, 100]);
        assert_eq!(got.union, 200);
    }

    /// A file may map the same physical run twice. The union counts it
    /// once and it stays exclusive to that file.
    #[test]
    fn a_file_mapping_a_run_twice_still_holds_it_alone() {
        let got = occupancy(&[vec![ext(0, 100), ext(0, 100)]]);
        assert_eq!(got.exclusive, vec![100]);
        assert_eq!(got.union, 100);
    }

    #[test]
    fn empty_input_and_empty_files_are_zero() {
        assert_eq!(occupancy(&[]).union, 0);
        let got = occupancy(&[vec![], vec![ext(0, 0)]]);
        assert_eq!(got.exclusive, vec![0, 0]);
        assert_eq!(got.union, 0);
    }

    /// Partial overlaps split into three regions: mine, ours, theirs.
    #[test]
    fn partial_overlap_splits_into_three_regions() {
        let got = occupancy(&[vec![ext(0, 100)], vec![ext(60, 100)]]);
        assert_eq!(got.exclusive, vec![60, 60]);
        assert_eq!(got.union, 160);
    }
}
