pub mod cp;
pub mod debug;
pub mod exec;
pub mod image;
pub mod info;
pub mod init;
pub mod kernel;
pub mod snapshot;
pub mod ssh;
pub mod vm;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "ember",
    about = "Lightweight VM manager with copy-on-write storage"
)]
#[command(version)]
pub struct Cli {
    /// Override state directory
    ///
    /// Default: /var/lib/ember (Linux), ~/Library/Application Support/ember (macOS)
    #[arg(long, global = true, default_value_os_t = default_state_dir())]
    pub state_dir: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

/// Platform-appropriate default state directory.
///
/// Linux: `/var/lib/ember` (requires root, alongside ZFS datasets).
/// macOS: `~/Library/Application Support/ember/` (no root, APFS clones).
fn default_state_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("ember");
        }
    }
    PathBuf::from("/var/lib/ember")
}

#[derive(Subcommand)]
pub enum Command {
    /// Initialize ember: set up storage, create state directory, download kernel
    Init(init::InitArgs),

    /// Manage virtual machines
    #[command(subcommand)]
    Vm(vm::VmCommand),

    /// Manage images
    #[command(subcommand)]
    Image(image::ImageCommand),

    /// Build and manage kernels
    #[command(subcommand)]
    Kernel(kernel::KernelCommand),

    /// Manage VM snapshots
    #[command(subcommand)]
    Snapshot(snapshot::SnapshotCommand),

    /// SSH into a VM
    Ssh(ssh::SshArgs),

    /// Execute a command in a VM
    Exec(exec::ExecArgs),

    /// Copy files between host and VM
    Cp(cp::CpArgs),

    /// Show ember configuration and status overview
    Info,

    /// Debugging and diagnostics
    #[command(subcommand)]
    Debug(debug::DebugCommand),

    /// Reconcile internal state with actual Firecracker process state
    Reconcile,

    /// Print version information
    Version,
}
