//! Platform-specific backend implementations.
//!
//! Traits and shared types are defined in `ember_core::backend`.
//! This module re-exports them and provides the type aliases that
//! select the active platform backend at compile time.

// Re-export all traits and shared types from ember-core.
pub use ember_core::backend::*;

// Re-export platform-specific implementations from their crates.
#[cfg(target_os = "linux")]
pub use ember_linux as linux;

#[cfg(target_os = "macos")]
pub mod macos;

// Type aliases for the active platform backend.
// Selected at compile time based on target OS.
#[cfg(target_os = "linux")]
pub type Vm = ember_linux::LinuxVm;
#[cfg(target_os = "linux")]
pub type Storage = ember_linux::LinuxStorage;
#[cfg(target_os = "linux")]
pub type Network = ember_linux::LinuxNetwork;

#[cfg(target_os = "macos")]
pub type Vm = macos::MacosVm;
#[cfg(target_os = "macos")]
pub type Storage = macos::MacosStorage;
#[cfg(target_os = "macos")]
pub type Network = macos::MacosNetwork;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("ember only supports Linux and macOS");
