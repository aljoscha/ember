mod cli;
pub mod error;
pub mod image;
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

    match &cli.command {
        Command::Init(args) => cli::init::run(args, &cli.state_dir),
        Command::Vm(cmd) => cli::vm::run(cmd),
        Command::Image(cmd) => cli::image::run(cmd, &cli.state_dir),
        Command::Snapshot(cmd) => cli::snapshot::run(cmd),
        Command::Exec(args) => cli::exec::run(args),
        Command::Cp(args) => cli::cp::run(args),
        Command::Version => {
            println!("ember {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
