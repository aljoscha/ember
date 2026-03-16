//! Platform-specific backend implementations.
//!
//! Traits and shared types are defined in `ember_core::backend`.
//! This module re-exports them and provides the platform-specific
//! implementations + type aliases.

// Re-export all traits and shared types from ember-core.
pub use ember_core::backend::*;

// Platform-specific implementations.
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

// Type aliases for the active platform backend.
// Selected at compile time based on target OS.
#[cfg(target_os = "linux")]
pub type Vm = linux::LinuxVm;
#[cfg(target_os = "linux")]
pub type Storage = linux::LinuxStorage;
#[cfg(target_os = "linux")]
pub type Network = linux::LinuxNetwork;

#[cfg(target_os = "macos")]
pub type Vm = macos::MacosVm;
#[cfg(target_os = "macos")]
pub type Storage = macos::MacosStorage;
#[cfg(target_os = "macos")]
pub type Network = macos::MacosNetwork;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("ember only supports Linux and macOS");
