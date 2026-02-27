//! SSH client for guest VM access.
//!
//! Provides SSH connectivity to Firecracker VMs for command execution,
//! file transfer, and interactive sessions. All guest interaction goes
//! over SSH using keys injected at VM creation time.

pub mod client;
