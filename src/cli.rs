pub mod cp;
pub mod deinit;
pub mod exec;
pub(crate) mod fmt;
pub mod image;
pub mod info;
pub mod init;
pub mod kernel;
pub mod ssh;
pub mod storage;
pub mod vm;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::backend::Platform;

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
    crate::backend::CurrentPlatform::default_state_dir()
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

    /// SSH into a VM
    Ssh(ssh::SshArgs),

    /// Execute a command in a VM
    Exec(exec::ExecArgs),

    /// Copy files between host and VM
    Cp(cp::CpArgs),

    /// Show ember configuration and status overview
    Info,

    /// Reconcile internal state with actual VM process state
    Reconcile,

    /// Tear down ember (inverse of `ember init`)
    Deinit(deinit::DeinitArgs),

    /// Storage pool administration
    #[command(subcommand)]
    Storage(storage::StorageCommand),

    /// Print version information
    Version,
}
