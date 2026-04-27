pub mod dm_thin;
pub mod firecracker;
pub mod image;
pub mod network;
pub mod network_backend;
pub mod platform;
pub mod reconcile;
pub mod storage;
pub mod vm;
pub mod zfs;
pub mod zvol;

pub use network_backend::LinuxNetwork;
pub use platform::LinuxPlatform;
pub use storage::LinuxStorage;
pub use vm::LinuxVm;

use std::sync::Arc;

use ember_core::backend::{InitConfig, StorageBackend};
use ember_core::config::GlobalConfig;
use ember_core::error::Result;

/// Construct the active storage backend.
///
/// Returns the implementation indicated by [`GlobalConfig::storage_backend`].
/// Currently only ZFS is wired up; btrfs and dm-thin variants are added in
/// later phases of the multi-backend rollout.
pub fn create_storage(config: &GlobalConfig) -> Arc<dyn StorageBackend> {
    Arc::new(LinuxStorage::new(config))
}

/// Initialize storage during `ember init`.
///
/// Dispatches to the concrete backend's `init` associated function. The
/// trait object is unavailable here because the backend hasn't been
/// constructed yet.
pub fn init_storage(config: &InitConfig) -> Result<()> {
    LinuxStorage::init(config)
}
