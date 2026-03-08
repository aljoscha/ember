//! File-based state management with JSON serialization and file locking.

#[cfg(target_os = "linux")]
pub mod reconcile;
pub mod store;
pub mod vm;
