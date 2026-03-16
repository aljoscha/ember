//! Ext4 filesystem image creation — re-exports from the platform backend.
//!
//! The actual implementation lives in the platform-specific crate:
//!   - Linux: `ember_linux::image` (loop mount + umount)
//!   - macOS: `backend::macos::image` (hdiutil attach/detach)
//!
//! This module re-exports the active platform's functions so existing
//! call sites (`image::ext4::create`, `image::ext4::estimate_size_mib`)
//! continue to work unchanged.

#[cfg(target_os = "linux")]
pub use ember_linux::image::*;

#[cfg(target_os = "macos")]
pub use crate::backend::macos::image::*;
