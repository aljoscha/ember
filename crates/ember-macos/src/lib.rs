pub mod image;
pub mod network;
pub mod platform;
pub mod reconcile;
pub mod storage;
pub mod vm;

pub use network::MacosNetwork;
pub use platform::MacosPlatform;
pub use storage::MacosStorage;
pub use vm::MacosVm;

use std::sync::Arc;

use ember_core::backend::{InitConfig, StorageBackend};
use ember_core::config::GlobalConfig;
use ember_core::error::Result;

/// Construct the active storage backend.
pub fn create_storage(config: &GlobalConfig) -> Arc<dyn StorageBackend> {
    Arc::new(MacosStorage::new(config))
}

/// Initialize storage during `ember init`.
pub fn init_storage(config: &InitConfig) -> Result<()> {
    MacosStorage::init(config)
}
