mod cli;
pub mod error;
pub mod firecracker;
pub mod image;
pub mod network;
pub mod ssh;
pub mod state;
pub mod zfs;

use clap::Parser;
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // version doesn't need root
    if !matches!(&cli.command, Command::Version) {
        require_root()?;
    }

    // Lightweight state reconciliation on every command (except init/version).
    // Cleans up after crashes: marks dead VMs stopped, removes orphaned TAP devices.
    if !matches!(&cli.command, Command::Version | Command::Init(_)) {
        state::reconcile::run(&cli.state_dir);
    }

    match &cli.command {
        Command::Init(args) => cli::init::run(args, &cli.state_dir),
        Command::Vm(cmd) => cli::vm::run(cmd, &cli.state_dir),
        Command::Image(cmd) => cli::image::run(cmd, &cli.state_dir),
        Command::Snapshot(cmd) => cli::snapshot::run(cmd),
        Command::Exec(args) => cli::exec::run(args, &cli.state_dir),
        Command::Cp(args) => cli::cp::run(args, &cli.state_dir),
        Command::Version => {
            println!("ember {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
