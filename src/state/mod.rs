//! File-based state management with JSON serialization and file locking.

#[cfg(target_os = "linux")]
pub mod reconcile;
#[cfg(target_os = "macos")]
pub mod reconcile_macos;
pub mod store;
pub mod vm;
