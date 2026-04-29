pub mod size;
pub mod vm;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which storage backend is active.
///
/// On Linux, runtime-selected at `ember init` and serialized to `config.json`.
/// Older configs without this field default to [`StorageKind::Zfs`] for
/// backwards compatibility.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageKind {
    #[default]
    Zfs,
    Btrfs,
    DmThin,
}

impl std::str::FromStr for StorageKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "zfs" => Ok(Self::Zfs),
            "btrfs" => Ok(Self::Btrfs),
            "dm-thin" | "dmthin" | "dm_thin" => Ok(Self::DmThin),
            other => Err(format!(
                "unknown storage backend '{other}' (expected zfs, btrfs, or dm-thin)"
            )),
        }
    }
}

/// How the dm-thin pool's data device is provided.
///
/// Resolved at `ember init` from the `--storage-path` argument and
/// persisted on `GlobalConfig` so reactivation does not depend on a
/// runtime filesystem probe — `is_dir()` could disagree with init if
/// the directory was removed, or a raw device replaced a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DmThinMode {
    /// `--storage-path` is a directory holding `metadata.img`/`data.img`.
    File,
    /// `--storage-path` is a raw block device used as the data device.
    /// Metadata then lives under `state_dir/dm-thin-metadata.img`.
    RawDevice,
}

/// Global configuration written by `ember init`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// Storage backend selected at init time.
    /// Defaults to [`StorageKind::Zfs`] for older configs without this field.
    #[serde(default)]
    pub storage_backend: StorageKind,
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
    /// Backing path for non-ZFS backends.
    ///
    /// * btrfs: block device or sparse image file containing the btrfs filesystem.
    /// * dm-thin: directory holding `metadata.img`/`data.img`, or a raw block device.
    /// * ZFS: unused.
    #[serde(default)]
    pub storage_path: Option<PathBuf>,
    /// dm-thin pool block size in 512-byte sectors. Permanent at pool
    /// creation, so `ember init` resolves the user flag (or default) at
    /// init time and persists the actual value here. `None` means "use
    /// the backend default" — only legacy configs predating this
    /// resolution should hit that branch; new configs always pin the
    /// value the running pool was created with.
    #[serde(default)]
    pub dm_thin_block_size: Option<u32>,
    /// dm-thin pool layout: file-backed (sparse files inside
    /// `storage_path`) or raw-device (`storage_path` is a block device).
    /// Resolved at `ember init` and persisted so reactivation does not
    /// rely on a live `is_dir()` probe. `None` on legacy configs and on
    /// non-dm-thin backends.
    #[serde(default)]
    pub dm_thin_mode: Option<DmThinMode>,
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
