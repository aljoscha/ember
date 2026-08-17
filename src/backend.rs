//! Platform-specific backend implementations.
//!
//! Traits and shared types are defined in `ember_core::backend`.
//! This module re-exports them and provides the type aliases that
//! select the active platform backend at compile time.

use std::sync::Arc;

// Re-export all traits and shared types from ember-core.
pub use ember_core::backend::*;

// Re-export platform-specific implementations from their crates.
#[cfg(target_os = "linux")]
pub use ember_linux as linux;

#[cfg(target_os = "macos")]
pub use ember_macos as macos;

// Type aliases for the active platform backend.
// `Vm`, `Network`, and `CurrentPlatform` are selected at compile time
// via `#[cfg(target_os)]`. `Storage` is a runtime trait object so the
// concrete implementation can be picked from `GlobalConfig` (e.g., ZFS
// vs btrfs vs dm-thin on Linux).
#[cfg(target_os = "linux")]
pub type Vm = ember_linux::LinuxVm;
#[cfg(target_os = "linux")]
pub type Network = ember_linux::LinuxNetwork;

#[cfg(target_os = "macos")]
pub type Vm = ember_macos::MacosVm;
#[cfg(target_os = "macos")]
pub type Network = ember_macos::MacosNetwork;

pub type Storage = Arc<dyn StorageBackend>;

#[cfg(target_os = "linux")]
pub use ember_linux::{create_storage, init_storage, try_create_storage};
#[cfg(target_os = "macos")]
pub use ember_macos::{create_storage, init_storage, try_create_storage};

/// Best-effort space accounting, for commands where storage is not the
/// subject: `vm list`, `vm inspect`, `info`.
///
/// Those must keep working when storage cannot be measured, since one
/// common reason to run them is that storage is broken. Callers render
/// a dash for whatever comes back missing. `ember storage usage` is the
/// strict counterpart and reports the error instead.
///
/// Note that the maps are restricted to the records passed in while the
/// pool figures are always installation-wide, so a caller asking about
/// one VM still gets whole-pool capacity.
pub fn try_usage(
    config: &ember_core::config::GlobalConfig,
    vms: &[ember_core::state::vm::VmMetadata],
    images: &[ember_core::image::registry::ImageEntry],
) -> Option<StorageUsage> {
    try_create_storage(config).ok()?.usage(vms, images).ok()
}

#[cfg(target_os = "linux")]
pub type CurrentPlatform = ember_linux::LinuxPlatform;
#[cfg(target_os = "macos")]
pub type CurrentPlatform = ember_macos::MacosPlatform;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("ember only supports Linux and macOS");
