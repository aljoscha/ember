//! Shared types, traits, and utilities for ember.
//!
//! This crate contains the platform-agnostic foundation:
//! - Backend trait definitions (`VmBackend`, `StorageBackend`, `NetworkBackend`)
//! - Error types and state management
//! - SSH client, image injection, kernel presets
//! - Network IP allocation
//!
//! Platform-specific implementations live in `ember-linux` and `ember-macos`.

pub mod backend;
pub mod cleanup;
pub mod config;
pub mod error;
pub mod image;
pub mod kernel;
pub mod network;
pub mod ssh;
pub mod state;
