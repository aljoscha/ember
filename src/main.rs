mod cli;
pub mod config;
pub mod error;
pub mod firecracker;
pub mod image;
pub mod network;
pub mod ssh;
pub mod state;
pub mod zfs;

use clap::Parser;
use cli::{Cli, Command};
use cli::vm::VmCommand;

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
/// SSH-based commands (exec, cp, vm ssh) only read VM state and invoke the
/// system SSH client — no root required. Read-only queries (vm list, vm
/// inspect) also work without elevated privileges.
fn needs_root(command: &Command) -> bool {
    match command {
        Command::Version => false,
        Command::Exec(_) | Command::Cp(_) => false,
        Command::Vm(VmCommand::Ssh(_) | VmCommand::List(_) | VmCommand::Inspect(_)) => false,
        _ => true,
    }
}

/// Returns true for commands that should trigger state reconciliation.
///
/// Reconciliation cleans up after crashes (dead VMs, orphaned TAP devices)
/// and requires root. Skip it for read-only and SSH-client commands.
fn needs_reconcile(command: &Command) -> bool {
    match command {
        Command::Version | Command::Init(_) => false,
        Command::Exec(_) | Command::Cp(_) => false,
        Command::Vm(VmCommand::Ssh(_) | VmCommand::List(_) | VmCommand::Inspect(_)) => false,
        _ => true,
    }
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
        Command::Exec(args) => cli::exec::run(args, &cli.state_dir),
        Command::Cp(args) => cli::cp::run(args, &cli.state_dir),
        Command::Version => {
            println!("ember {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
