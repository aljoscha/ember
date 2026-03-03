pub mod cp;
pub mod exec;
pub mod image;
pub mod init;
pub mod snapshot;
pub mod vm;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ember", about = "Lightweight Firecracker VM manager with ZFS-backed storage")]
#[command(version)]
pub struct Cli {
    /// Override state directory (default: /var/lib/ember)
    #[arg(long, global = true, default_value = "/var/lib/ember")]
    pub state_dir: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Initialize ember: create/verify ZFS pool, datasets, download kernel
    Init(init::InitArgs),

    /// Manage virtual machines
    #[command(subcommand)]
    Vm(vm::VmCommand),

    /// Manage images
    #[command(subcommand)]
    Image(image::ImageCommand),

    /// Manage VM snapshots
    #[command(subcommand)]
    Snapshot(snapshot::SnapshotCommand),

    /// Execute a command in a VM
    Exec(exec::ExecArgs),

    /// Copy files between host and VM
    Cp(cp::CpArgs),

    /// Print version information
    Version,
}
