mod cleanup;
mod cli;
pub mod config;
pub mod error;
pub mod firecracker;
pub mod image;
pub mod kernel;
pub mod network;
pub mod ssh;
pub mod state;
pub mod zfs;

use clap::Parser;
use cli::vm::VmCommand;
use cli::{Cli, Command};

/// Check that the process is running as root (euid 0).
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
            | Command::Ssh(_)
            | Command::Exec(_)
            | Command::Cp(_)
            | Command::Vm(VmCommand::List(_) | VmCommand::Inspect(_))
    )
}

/// Returns true for commands that should trigger state reconciliation.
///
/// Reconciliation cleans up after crashes (dead VMs, orphaned TAP devices)
/// and requires root. Skip it for read-only and SSH-client commands.
fn needs_reconcile(command: &Command) -> bool {
    !matches!(
        command,
        Command::Version
            | Command::Init(_)
            | Command::Ssh(_)
            | Command::Exec(_)
            | Command::Cp(_)
            | Command::Vm(VmCommand::List(_) | VmCommand::Inspect(_))
    )
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if needs_root(&cli.command) {
        require_root()?;
    }

    // Lightweight state reconciliation on every privileged command.
    // Cleans up after crashes: marks dead VMs stopped, removes orphaned TAP devices.
    if needs_reconcile(&cli.command) {
        state::reconcile::run(&cli.state_dir);
    }

    match &cli.command {
        Command::Init(args) => cli::init::run(args, &cli.state_dir),
        Command::Vm(cmd) => cli::vm::run(cmd, &cli.state_dir),
        Command::Image(cmd) => cli::image::run(cmd, &cli.state_dir),
        Command::Snapshot(cmd) => cli::snapshot::run(cmd, &cli.state_dir),
        Command::Ssh(args) => cli::ssh::run(args, &cli.state_dir),
        Command::Exec(args) => cli::exec::run(args, &cli.state_dir),
        Command::Cp(args) => cli::cp::run(args, &cli.state_dir),
        Command::Version => {
            println!("ember {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
