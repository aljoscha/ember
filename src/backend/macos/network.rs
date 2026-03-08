//! macOS network backend: vmnet shared mode.
//!
//! vmnet provides NAT + DHCP automatically — most operations are no-ops.
//! The main work is discovering the guest IP from DHCP leases after boot.

use crate::backend::NetworkBackend;
use crate::cli::init::GlobalConfig;
use crate::error::Result;
use crate::state::vm::{NetworkInfo, VmMetadata};

/// macOS network backend using vmnet (shared mode).
///
/// vmnet handles NAT and DHCP internally, so setup/teardown are no-ops.
pub struct MacosNetwork;

impl MacosNetwork {
    pub fn new(_store: crate::state::store::StateStore) -> Self {
        Self
    }
}

impl NetworkBackend for MacosNetwork {
    fn setup(&self, _vm: &VmMetadata, _config: &GlobalConfig) -> Result<NetworkInfo> {
        todo!("macOS: vmnet network setup")
    }

    fn teardown(&self, _vm: &VmMetadata) -> Result<()> {
        // vmnet cleans up automatically — nothing to do.
        Ok(())
    }

    fn discover_guest_ip(&self, _mac: &str) -> Result<String> {
        todo!("macOS: discover guest IP from DHCP leases")
    }
}
