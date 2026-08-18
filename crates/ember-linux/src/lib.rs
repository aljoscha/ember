pub mod cgroup;
pub mod dm;
pub mod dm_thin;
pub mod dm_thin_storage;
pub mod firecracker;
pub mod image;
pub mod network;
pub mod network_backend;
pub mod platform;
pub mod reconcile;
pub mod storage;
pub mod vdo;
pub mod vm;
pub mod zfs;
pub mod zvol;

pub use dm_thin_storage::DmThinStorage;
pub use network_backend::LinuxNetwork;
pub use platform::LinuxPlatform;
pub use storage::LinuxStorage;
pub use vm::LinuxVm;

use std::sync::Arc;

use ember_core::backend::{InitConfig, StorageBackend};
use ember_core::config::{GlobalConfig, StorageKind};
use ember_core::error::{Error, Result};

/// Construct the active storage backend, or explain why we cannot.
///
/// btrfs is not yet implemented. Rather than silently routing through
/// the ZFS path with garbage inputs, a hand-edited `config.json` naming
/// it gets a real error. `init_storage` returns the same shape of error
/// from the init side.
pub fn try_create_storage(config: &GlobalConfig) -> Result<Arc<dyn StorageBackend>> {
    match config.storage_backend {
        StorageKind::Zfs => Ok(Arc::new(LinuxStorage::new(config))),
        StorageKind::DmThin => Ok(Arc::new(DmThinStorage::new(config))),
        StorageKind::Btrfs => Err(Error::Config(
            "config.json has storage_backend = btrfs, but the btrfs \
             storage backend is not implemented"
                .to_string(),
        )),
    }
}

/// [`try_create_storage`] for callers that are about to operate on
/// storage anyway, where an unusable backend is fatal regardless.
///
/// Read-only paths that must survive a broken config use the fallible
/// form instead.
pub fn create_storage(config: &GlobalConfig) -> Arc<dyn StorageBackend> {
    try_create_storage(config).unwrap_or_else(|e| panic!("{e}"))
}

/// Initialize storage during `ember init`.
///
/// Dispatches to the concrete backend's `init` associated function. The
/// trait object is unavailable here because the backend hasn't been
/// constructed yet.
pub fn init_storage(config: &InitConfig) -> Result<()> {
    match config.storage_backend {
        StorageKind::Zfs => LinuxStorage::init(config),
        StorageKind::DmThin => DmThinStorage::init(config),
        StorageKind::Btrfs => Err(Error::Config(
            "btrfs storage backend is not yet implemented".to_string(),
        )),
    }
}
