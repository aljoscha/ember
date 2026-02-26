mod cli;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

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
