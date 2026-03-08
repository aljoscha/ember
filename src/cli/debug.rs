//! Debug commands for inspecting ember internals.

use std::path::Path;

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

    // Get actual disk usage via df on the state directory.
    let actual_used = get_volume_used_bytes(state_dir);

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
        println!("Actual disk used:  {} (via df)", format_bytes(used));
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

/// Get the "used" bytes on the volume containing the given path.
///
/// Runs `df -k <path>` and parses the output. Returns `None` if the
/// command fails or output can't be parsed.
fn get_volume_used_bytes(path: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // df -k output:
    // Filesystem  1024-blocks  Used  Available  Capacity  Mounted on
    // /dev/disk1  ...          USED  ...        ...       ...
    let line = stdout.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    // Field 2 is "Used" in 1024-byte blocks.
    let used_kb: u64 = fields.get(2)?.parse().ok()?;
    Some(used_kb * 1024)
}

/// Format bytes as a human-readable string (e.g., "2.1 GB").
fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    const KB: f64 = 1_000.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}
