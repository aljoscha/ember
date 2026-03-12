pub mod backend;
mod cleanup;
mod cli;
pub mod config;
pub mod error;
#[cfg(target_os = "linux")]
pub mod firecracker;
pub mod image;
pub mod kernel;
pub mod network;
pub mod ssh;
pub mod state;
#[cfg(target_os = "linux")]
pub mod zfs;

use clap::Parser;
#[cfg(target_os = "linux")]
use cli::kernel::KernelCommand;
use cli::vm::VmCommand;
use cli::{Cli, Command};

/// Check that the process is running as root (euid 0).
/// Only needed on Linux where ZFS, TAP, and iptables require root.
#[cfg(target_os = "linux")]
fn require_root() -> anyhow::Result<()> {
    if !nix::unistd::geteuid().is_root() {
        anyhow::bail!(
            "ember requires root privileges.\n\
             Hint: run with sudo, e.g.  sudo ember <command>"
        );
    }
    Ok(())
}

/// Returns true for commands that don't need root privileges.
///
/// SSH-based commands (ssh, exec, cp) only read VM state and invoke the
/// system SSH client — no root required. Read-only queries (vm list, vm
/// inspect) also work without elevated privileges.
#[cfg(target_os = "linux")]
fn needs_root(command: &Command) -> bool {
    !matches!(
        command,
        Command::Version
            | Command::Info
            | Command::Debug(_)
            | Command::Ssh(_)
            | Command::Exec(_)
            | Command::Cp(_)
            | Command::Vm(VmCommand::List(_) | VmCommand::Inspect(_))
            | Command::Kernel(KernelCommand::List)
    )
}

/// Returns true for commands that should trigger state reconciliation.
///
/// Reconciliation cleans up after crashes (dead VMs, orphaned resources)
/// and may require root on Linux. Skip it for read-only and SSH-client commands.
fn needs_reconcile(command: &Command) -> bool {
    !matches!(
        command,
        Command::Version
            | Command::Info
            | Command::Init(_)
            | Command::Debug(_)
            | Command::Reconcile
            | Command::Ssh(_)
            | Command::Exec(_)
            | Command::Cp(_)
            | Command::Vm(VmCommand::List(_) | VmCommand::Inspect(_))
            | Command::Kernel(_)
    )
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Linux requires root for ZFS, TAP, and iptables operations.
    // macOS runs entirely without root (vmnet, APFS clones).
    #[cfg(target_os = "linux")]
    if needs_root(&cli.command) {
        require_root()?;
    }

    // Lightweight state reconciliation on every privileged command.
    // Linux: cleans up dead VMs, orphaned TAP devices. Requires root.
    // macOS: cleans up dead VMs, releases orphaned IP allocations.
    #[cfg(target_os = "linux")]
    if needs_reconcile(&cli.command) {
        state::reconcile::run(&cli.state_dir);
    }
    #[cfg(target_os = "macos")]
    if needs_reconcile(&cli.command) {
        state::reconcile_macos::run(&cli.state_dir);
    }

    match &cli.command {
        Command::Init(args) => cli::init::run(args, &cli.state_dir),
        Command::Vm(cmd) => cli::vm::run(cmd, &cli.state_dir),
        Command::Image(cmd) => cli::image::run(cmd, &cli.state_dir),
        Command::Kernel(cmd) => cli::kernel::run(cmd, &cli.state_dir),
        Command::Snapshot(cmd) => cli::snapshot::run(cmd, &cli.state_dir),
        Command::Ssh(args) => cli::ssh::run(args, &cli.state_dir),
        Command::Exec(args) => cli::exec::run(args, &cli.state_dir),
        Command::Cp(args) => cli::cp::run(args, &cli.state_dir),
        Command::Info => cli::info::run(&cli.state_dir),
        Command::Debug(cmd) => cli::debug::run(cmd, &cli.state_dir),
        Command::Reconcile => {
            #[cfg(target_os = "linux")]
            state::reconcile::run(&cli.state_dir);
            #[cfg(target_os = "macos")]
            state::reconcile_macos::run(&cli.state_dir);
            Ok(())
        }
        Command::Version => {
            println!("ember {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
