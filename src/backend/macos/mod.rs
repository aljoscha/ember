//! macOS backend implementations: AVF (ember-vz) + APFS clones + vmnet.

pub mod image;
pub mod network;
pub mod storage;
pub mod vm;

pub use network::MacosNetwork;
pub use storage::MacosStorage;
pub use vm::MacosVm;
