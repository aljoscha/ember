pub mod size;
pub mod vm;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Global configuration written by `ember init`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub pool: String,
    pub dataset: String,
    pub kernel_path: Option<PathBuf>,
    /// Default WAN interface for iptables NAT rules.
    /// Auto-detected during `ember init`, overridable via `--wan-iface`.
    #[serde(default)]
    pub wan_iface: Option<String>,
    /// State directory path. Used by macOS backend to derive storage paths.
    /// Populated during `ember init`; defaults to empty path for backwards compat.
    #[serde(default)]
    pub state_dir: PathBuf,
}

impl GlobalConfig {
    /// Full ZFS dataset path for images (e.g. `ember/ember/images`).
    pub fn images_dataset(&self) -> String {
        format!("{}/{}/images", self.pool, self.dataset)
    }

    /// Full ZFS dataset path for VMs (e.g. `ember/ember/vms`).
    pub fn vms_dataset(&self) -> String {
        format!("{}/{}/vms", self.pool, self.dataset)
    }
}
