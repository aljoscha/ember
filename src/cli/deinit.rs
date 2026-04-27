//! `ember deinit` — tear down the storage backend.
//!
//! The inverse of `ember init`. Refuses to run while VMs are alive
//! to avoid leaving the user with a half-destroyed pool.

use std::fs;
use std::path::Path;

use clap::Args;

use crate::backend::create_storage;
use ember_core::config::GlobalConfig;
use ember_core::state::store::StateStore;
use ember_core::state::vm;

#[derive(Args)]
pub struct DeinitArgs {
    /// Also delete backing files (dm-thin metadata.img/data.img) so
    /// a future `ember init` starts from scratch. Block devices
    /// supplied via `--storage-path` are always left intact.
    #[arg(long)]
    pub purge: bool,
}

pub fn run(args: &DeinitArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = match store.read_optional(&store.config_path())? {
        Some(c) => c,
        None => {
            println!("ember is not initialized — nothing to tear down.");
            return Ok(());
        }
    };

    // Refuse to deinit if any VM is recorded. Forces the user to
    // `ember vm delete` (or `--force`) first so that backend cleanup
    // doesn't leave dangling per-VM resources.
    let vms = vm::list(&store).unwrap_or_default();
    if !vms.is_empty() {
        let names: Vec<String> = vms.into_iter().map(|v| v.name).collect();
        anyhow::bail!(
            "refusing to deinit while {} VM(s) are registered: {}\n\
             Hint: delete them first with 'ember vm delete <name>'.",
            names.len(),
            names.join(", "),
        );
    }

    let storage = create_storage(&config);
    storage.deinit(args.purge)?;

    // Remove the persisted config last — the backend may have needed
    // it to find backing paths.
    let config_path = store.config_path();
    if config_path.exists() {
        fs::remove_file(&config_path).map_err(|e| ember_core::error::Error::Io {
            path: config_path,
            source: e,
        })?;
    }

    println!("ember deinitialized.");
    Ok(())
}
