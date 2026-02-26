mod cli;
pub mod error;

use clap::Parser;
use cli::{Cli, Command};

/// Check that the process is running as root (euid 0).
fn require_root() -> anyhow::Result<()> {
    if !nix::unistd::geteuid().is_root() {
        anyhow::bail!(
            "crackling requires root privileges.\n\
             Hint: run with sudo, e.g.  sudo crackling <command>"
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
        Command::Init(args) => cli::init::run(args),
        Command::Vm(cmd) => cli::vm::run(cmd),
        Command::Image(cmd) => cli::image::run(cmd),
        Command::Snapshot(cmd) => cli::snapshot::run(cmd),
        Command::Exec(args) => cli::exec::run(args),
        Command::Cp(args) => cli::cp::run(args),
        Command::Version => {
            println!("crackling {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}
