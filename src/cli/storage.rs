//! `ember storage` subcommands: pool-level administration.

use std::path::Path;

use clap::{Args, Subcommand};

use crate::backend::create_storage;
use ember_core::config::size::ByteSize;
use ember_core::config::GlobalConfig;
use ember_core::state::store::StateStore;

#[derive(Subcommand)]
pub enum StorageCommand {
    /// Grow the underlying pool capacity (dm-thin only).
    Grow(GrowArgs),
}

#[derive(Args)]
pub struct GrowArgs {
    /// New total size for the data device, e.g. `100G`. Must be larger
    /// than the current size.
    #[arg(long)]
    pub size: ByteSize,
}

pub fn run(cmd: &StorageCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        StorageCommand::Grow(args) => grow(args, state_dir),
    }
}

fn grow(args: &GrowArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = create_storage(&config);
    storage.grow(args.size)?;
    Ok(())
}
