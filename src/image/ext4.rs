//! Ext4 filesystem image creation — re-exports from the platform crate.
//!
//! The actual implementation lives in the platform-specific crate:
//!   - Linux: `ember_linux::image` (loop mount + umount)
//!   - macOS: `ember_macos::image` (mkfs.ext4 -d)
//!
//! This module re-exports the active platform's functions so existing
//! call sites (`image::ext4::create`, `image::ext4::estimate_size_mib`)
//! continue to work unchanged.

#[cfg(target_os = "linux")]
pub use ember_linux::image::*;

#[cfg(target_os = "macos")]
pub use ember_macos::image::*;
