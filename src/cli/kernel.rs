//! `ember kernel` subcommands: build and list kernels.

use std::io::Write;
use std::path::Path;

use clap::{Args, Subcommand};

use super::fmt::format_bytes_binary;
use crate::kernel;

#[derive(Subcommand)]
pub enum KernelCommand {
    /// Build a custom kernel with Docker networking support
    Build(BuildArgs),

    /// List available kernels in the state directory
    List,
}

#[derive(Args)]
pub struct BuildArgs {
    /// Number of parallel make jobs (default: all cores)
    #[arg(long, short = 'j')]
    pub jobs: Option<usize>,

    /// Skip confirmation prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
}

pub fn run(cmd: &KernelCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        KernelCommand::Build(args) => build(args, state_dir),
        KernelCommand::List => list(state_dir),
    }
}

fn build(args: &BuildArgs, state_dir: &Path) -> anyhow::Result<()> {
    let tool = kernel::build::detect_container_tool()?;
    let jobs = args.jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    });

    if !args.yes {
        println!(
            "This will build a Linux kernel with Docker networking and AVF support.\n\
             \n\
             \x20 Kernel source:  Amazon Linux 6.1.163 (shallow clone, ~1 GB download)\n\
             \x20 Base config:    Firecracker CI x86_64 6.1\n\
             \x20 Extra config:   iptables raw, nftables, dummy interface (Docker)\n\
             \x20                  virtio-pci, virtio-console, ip=dhcp (AVF)\n\
             \x20 Build method:   container ({tool})\n\
             \x20 Parallelism:    make -j{jobs}\n\
             \n\
             The kernel source download is ~1 GB and compilation may take 10-30 minutes."
        );

        if !confirm("\nProceed? [y/N]") {
            println!("Aborted.");
            return Ok(());
        }
        println!();
    }

    let store = crate::state::store::StateStore::new(state_dir.to_path_buf());
    let dest = kernel::build::build(&store, jobs, &tool)?;

    println!(
        "\nKernel ready. It is now the default kernel for new VMs.\n\
         To use explicitly: ember vm create <name> --image <image> --kernel docker\n\
         Installed at: {}",
        dest.display()
    );

    Ok(())
}

fn list(state_dir: &Path) -> anyhow::Result<()> {
    let store = crate::state::store::StateStore::new(state_dir.to_path_buf());
    let kernel_dir = store.kernel_dir();

    if !kernel_dir.exists() {
        println!("No kernels directory found at {}", kernel_dir.display());
        return Ok(());
    }

    let mut entries: Vec<_> = std::fs::read_dir(&kernel_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .collect();

    if entries.is_empty() {
        println!("No kernels found in {}", kernel_dir.display());
        return Ok(());
    }

    entries.sort_by_key(|e| e.file_name());

    let default_filename = kernel::DEFAULT_PRESET.filename();

    for entry in &entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let meta = entry.metadata()?;
        let size = format_bytes_binary(meta.len());

        // Check if this file corresponds to a known preset.
        let preset_label = if name_str == kernel::KernelPreset::Stock.filename() {
            " (stock)"
        } else if name_str == kernel::KernelPreset::Docker.filename() {
            " (docker)"
        } else {
            ""
        };

        let default_marker = if name_str == default_filename {
            " [default]"
        } else {
            ""
        };

        println!("{name_str}  {size}{preset_label}{default_marker}");
    }

    Ok(())
}

/// Prompt the user for confirmation, returning true for y/yes.
fn confirm(prompt: &str) -> bool {
    eprint!("{prompt} ");
    std::io::stderr().flush().ok();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).unwrap_or(0) == 0 {
        return false;
    }
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
