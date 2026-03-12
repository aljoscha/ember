//! Debug commands for inspecting ember internals.

use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum DebugCommand {
    /// Report CoW storage efficiency (logical vs actual disk usage)
    StorageEfficiency,
}

pub fn run(cmd: &DebugCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        DebugCommand::StorageEfficiency => storage_efficiency(state_dir),
    }
}

/// Report storage efficiency by comparing logical file sizes against
/// actual disk usage.
///
/// Logical size: sum of all `.img` file sizes via `stat` (what `du` reports).
/// Actual disk usage: free space delta on the volume, approximated by
/// subtracting current free space from volume capacity and comparing
/// against logical totals.
fn storage_efficiency(state_dir: &Path) -> anyhow::Result<()> {
    let images_dir = state_dir.join("images").join("data");
    let vms_dir = state_dir.join("vms");

    // Count images and their logical sizes.
    let (image_count, image_bytes) = count_img_files(&images_dir);

    // Count VM rootfs files and their logical sizes.
    let mut vm_count: u64 = 0;
    let mut vm_bytes: u64 = 0;
    let mut snap_count: u64 = 0;
    let mut snap_bytes: u64 = 0;

    if vms_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&vms_dir) {
            for entry in entries.flatten() {
                let vm_dir = entry.path();
                if !vm_dir.is_dir() {
                    continue;
                }

                // Count rootfs.img for this VM.
                let rootfs = vm_dir.join("rootfs.img");
                if let Ok(meta) = std::fs::metadata(&rootfs) {
                    vm_count += 1;
                    vm_bytes += meta.len();
                }

                // Count snapshot .img files.
                let snap_dir = vm_dir.join("snapshots");
                if snap_dir.exists() {
                    let (sc, sb) = count_img_files(&snap_dir);
                    snap_count += sc;
                    snap_bytes += sb;
                }
            }
        }
    }

    let total_logical = image_bytes + vm_bytes + snap_bytes;

    // Get actual disk usage by summing allocated blocks for all .img files.
    // On APFS, cloned files only report their unique (non-shared) blocks,
    // so this correctly reflects CoW savings.
    let actual_used = get_actual_disk_bytes(state_dir);

    println!();
    println!("Storage Efficiency Report");
    println!("{}", "─".repeat(40));
    println!(
        "Images:        {:>3} ({} logical)",
        image_count,
        format_bytes(image_bytes)
    );
    println!(
        "VMs:           {:>3} ({} logical)",
        vm_count,
        format_bytes(vm_bytes)
    );
    println!(
        "Snapshots:     {:>3} ({} logical)",
        snap_count,
        format_bytes(snap_bytes)
    );
    println!("                   {}", "─".repeat(22));
    println!("Total logical:     {}", format_bytes(total_logical));

    if let Some(used) = actual_used {
        println!("Actual disk used:  {}", format_bytes(used));
        if used > 0 && total_logical > used {
            let ratio = total_logical as f64 / used as f64;
            println!("CoW efficiency:    {:.1}x space savings", ratio);
        }
    } else {
        println!("Actual disk used:  (could not determine)");
    }
    println!();

    Ok(())
}

/// Count `.img` files in a directory and sum their logical sizes.
fn count_img_files(dir: &Path) -> (u64, u64) {
    let mut count: u64 = 0;
    let mut bytes: u64 = 0;

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("img") {
                if let Ok(meta) = std::fs::metadata(&path) {
                    count += 1;
                    bytes += meta.len();
                }
            }
        }
    }

    (count, bytes)
}

/// Get actual disk bytes used by all `.img` files under the state directory.
///
/// Uses `st_blocks` from file metadata, which reports 512-byte blocks
/// actually allocated on disk. On APFS, cloned files only count their
/// unique (non-shared) blocks, so this correctly reflects CoW savings.
#[cfg(unix)]
fn get_actual_disk_bytes(state_dir: &Path) -> Option<u64> {
    let mut total_blocks: u64 = 0;
    sum_img_blocks(state_dir, &mut total_blocks);
    // st_blocks counts 512-byte blocks.
    Some(total_blocks * 512)
}

#[cfg(not(unix))]
fn get_actual_disk_bytes(_state_dir: &Path) -> Option<u64> {
    None
}

/// Recursively walk a directory and sum `st_blocks` for all `.img` files.
#[cfg(unix)]
fn sum_img_blocks(dir: &Path, total: &mut u64) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sum_img_blocks(&path, total);
        } else if path.extension().and_then(|e| e.to_str()) == Some("img") {
            if let Ok(meta) = std::fs::metadata(&path) {
                *total += meta.blocks();
            }
        }
    }
}

use super::fmt::format_bytes_binary as format_bytes;
