pub mod backend;
mod cli;
pub mod image;

use clap::Parser;
use cli::kernel::KernelCommand;
use cli::vm::VmCommand;
use cli::{Cli, Command};

use crate::backend::{CurrentPlatform, Platform};

/// Check that the process is running as root (euid 0).
/// Only needed on platforms where storage/networking require root.
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

    // Check root if the platform requires it for privileged operations.
    if CurrentPlatform::REQUIRES_ROOT && needs_root(&cli.command) {
        require_root()?;
    }

    // Lightweight state reconciliation on every privileged command.
    if needs_reconcile(&cli.command) {
        CurrentPlatform::reconcile(&cli.state_dir);
    }

    match &cli.command {
        Command::Init(args) => cli::init::run(args, &cli.state_dir),
        Command::Vm(cmd) => cli::vm::run(cmd, &cli.state_dir),
        Command::Image(cmd) => cli::image::run(cmd, &cli.state_dir),
        Command::Kernel(cmd) => cli::kernel::run(cmd, &cli.state_dir),
        Command::Ssh(args) => cli::ssh::run(args, &cli.state_dir),
        Command::Exec(args) => cli::exec::run(args, &cli.state_dir),
        Command::Cp(args) => cli::cp::run(args, &cli.state_dir),
        Command::Info => cli::info::run(&cli.state_dir),
        Command::Debug(cmd) => cli::debug::run(cmd, &cli.state_dir),
        Command::Reconcile => {
            CurrentPlatform::reconcile(&cli.state_dir);
            Ok(())
        }
        Command::Deinit(args) => cli::deinit::run(args, &cli.state_dir),
        Command::Storage(cmd) => cli::storage::run(cmd, &cli.state_dir),
        Command::Version => {
            println!("ember {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
