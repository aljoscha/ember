use std::path::{Path, PathBuf};

use clap::Args;

use crate::backend::{init_storage, CurrentPlatform, InitConfig, Platform};
use ember_core::config::size::ByteSize;
use ember_core::config::{DmThinMode, GlobalConfig, StorageKind};
use ember_core::state::store::StateStore;

/// dm-thin pool block size (in 512-byte sectors) used when the user does
/// not pass `--block-size`. Resolved here at init time and persisted on
/// `GlobalConfig` so the value the running pool was created with stays
/// stable across ember upgrades — block size is permanent at pool
/// creation, and silently switching defaults later would orphan
/// existing pools.
#[cfg(target_os = "linux")]
const DM_THIN_DEFAULT_BLOCK_SIZE_SECTORS: u32 =
    ember_linux::dm_thin::pool::DEFAULT_BLOCK_SIZE_SECTORS;
#[cfg(not(target_os = "linux"))]
const DM_THIN_DEFAULT_BLOCK_SIZE_SECTORS: u32 = 128;

#[derive(Args)]
pub struct InitArgs {
    /// Storage backend: zfs (default) or dm-thin (Linux only)
    #[cfg_attr(target_os = "macos", arg(long, default_value = "zfs", hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long, default_value = "zfs"))]
    pub storage: StorageKind,

    /// ZFS pool name (--storage zfs only)
    #[cfg_attr(target_os = "macos", arg(long, default_value = "ember", hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long, default_value = "ember"))]
    pub pool: String,

    /// Block device for ZFS pool creation (--storage zfs only)
    #[cfg_attr(target_os = "macos", arg(long, hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long))]
    pub device: Option<String>,

    /// Dataset name within the pool (--storage zfs only)
    #[cfg_attr(target_os = "macos", arg(long, default_value = "ember", hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long, default_value = "ember"))]
    pub dataset: String,

    /// Backing path for non-ZFS backends (directory or block device).
    ///
    /// dm-thin: directory holding metadata.img/data.img, or a raw block
    /// device. Defaults to /var/lib/ember/dm-thin when omitted.
    #[arg(long)]
    pub storage_path: Option<PathBuf>,

    /// Pool size for file-backed dm-thin (e.g. `50G`). Required when
    /// `--storage-path` is a file path; ignored for raw block devices.
    #[arg(long)]
    pub size: Option<ByteSize>,

    /// Override metadata device size for dm-thin (e.g. `800M`).
    /// `thin_metadata_size` computes a recommended value when omitted.
    #[arg(long)]
    pub metadata_size: Option<ByteSize>,

    /// dm-thin pool block size in 512-byte sectors. Permanent at pool
    /// creation. Defaults to 128 (= 64 KiB).
    #[arg(long)]
    pub block_size: Option<u32>,

    /// Kernel preset or file path [presets: stock]
    #[arg(long)]
    pub kernel: Option<ember_core::kernel::KernelSpec>,

    /// WAN interface for NAT (auto-detected if not specified)
    #[arg(long)]
    pub wan_iface: Option<String>,
}

pub fn run(args: &InitArgs, state_dir: &Path) -> anyhow::Result<()> {
    // Refuse to switch backends silently. Existing configs win unless
    // the user runs `ember deinit` first.
    let store = StateStore::new(state_dir.to_path_buf());
    if let Ok(Some(existing)) = store.read_optional::<GlobalConfig>(&store.config_path()) {
        if existing.storage_backend != args.storage {
            anyhow::bail!(
                "ember is already initialized with the {:?} backend; \
                 run 'ember deinit' first to switch to {:?}",
                existing.storage_backend,
                args.storage,
            );
        }
    }

    // Resolve the dm-thin defaults so both InitConfig and GlobalConfig
    // see the same values.
    let storage_path = match args.storage {
        StorageKind::DmThin => Some(
            args.storage_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/var/lib/ember/dm-thin")),
        ),
        StorageKind::Btrfs => args.storage_path.clone(),
        StorageKind::Zfs => None,
    };

    // Resolve block size up-front for dm-thin so the persisted config
    // pins the value the pool was actually created with, even when the
    // user omits `--block-size`.
    let resolved_block_size = match args.storage {
        StorageKind::DmThin => Some(
            args.block_size
                .unwrap_or(DM_THIN_DEFAULT_BLOCK_SIZE_SECTORS),
        ),
        _ => args.block_size,
    };

    // Resolve file-vs-raw-device layout once and persist it. Doing this
    // here rather than in the backend keeps the contract explicit:
    // reactivation should not depend on a live `is_dir()` probe of
    // `storage_path` agreeing with what init saw.
    let resolved_dm_thin_mode = match (args.storage, storage_path.as_ref()) {
        (StorageKind::DmThin, Some(path)) => {
            if path.is_dir() || !path.exists() {
                Some(DmThinMode::File)
            } else {
                Some(DmThinMode::RawDevice)
            }
        }
        _ => None,
    };

    let init_config = InitConfig {
        storage_backend: args.storage,
        state_dir: state_dir.to_path_buf(),
        pool: args.pool.clone(),
        dataset: args.dataset.clone(),
        device: args.device.clone(),
        storage_path: storage_path.clone(),
        btrfs_size: None,
        dm_thin_size: args.size,
        dm_thin_metadata_size: args.metadata_size,
        dm_thin_block_size: resolved_block_size,
        dm_thin_mode: resolved_dm_thin_mode,
    };
    init_storage(&init_config)?;

    // Initialize state directory structure.
    store.init()?;
    println!("State directory initialized at {}", state_dir.display());

    // Download kernel if preset or path provided.
    let kernel_path = if let Some(spec) = &args.kernel {
        Some(spec.resolve(&store)?)
    } else {
        println!("No --kernel provided; a default kernel will be downloaded on first 'vm create'.");
        None
    };

    // Detect or use provided WAN interface.
    let (wan_iface, messages) = CurrentPlatform::detect_wan_iface(args.wan_iface.as_deref());
    for msg in &messages {
        println!("{msg}");
    }

    // Write config.
    let config = GlobalConfig {
        storage_backend: args.storage,
        pool: args.pool.clone(),
        dataset: args.dataset.clone(),
        kernel_path,
        wan_iface,
        state_dir: state_dir.to_path_buf(),
        storage_path,
        dm_thin_block_size: resolved_block_size,
        dm_thin_mode: resolved_dm_thin_mode,
    };
    store.write(&store.config_path(), &config)?;
    println!("Configuration written to {}", store.config_path().display());

    println!("\nember initialized successfully.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ember_core::config::StorageKind;
    use std::path::PathBuf;

    fn zfs_config(pool: &str, dataset: &str) -> GlobalConfig {
        GlobalConfig {
            storage_backend: StorageKind::Zfs,
            pool: pool.to_string(),
            dataset: dataset.to_string(),
            kernel_path: None,
            wan_iface: None,
            state_dir: PathBuf::default(),
            storage_path: None,
            dm_thin_block_size: None,
            dm_thin_mode: None,
        }
    }

    #[test]
    fn global_config_round_trip_with_kernel() {
        let config = GlobalConfig {
            kernel_path: Some(PathBuf::from("/var/lib/ember/kernels/vmlinux")),
            wan_iface: Some("eth0".to_string()),
            state_dir: PathBuf::from("/var/lib/ember"),
            ..zfs_config("testpool", "ember")
        };

        let json = serde_json::to_string(&config).unwrap();
        let loaded: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn global_config_round_trip_without_kernel() {
        let config = zfs_config("mypool", "mydata");
        let json = serde_json::to_string(&config).unwrap();
        let loaded: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn global_config_json_format() {
        let config = GlobalConfig {
            kernel_path: Some(PathBuf::from("/kernels/vmlinux")),
            wan_iface: Some("wlp2s0".to_string()),
            ..zfs_config("tank", "ember")
        };

        let json: serde_json::Value = serde_json::to_value(&config).unwrap();
        assert_eq!(json["pool"], "tank");
        assert_eq!(json["dataset"], "ember");
        assert_eq!(json["kernel_path"], "/kernels/vmlinux");
        assert_eq!(json["wan_iface"], "wlp2s0");
        assert_eq!(json["storage_backend"], "zfs");
    }

    #[test]
    fn global_config_null_kernel_in_json() {
        let config = zfs_config("tank", "ember");
        let json: serde_json::Value = serde_json::to_value(&config).unwrap();
        assert!(json["kernel_path"].is_null());
    }

    #[test]
    fn global_config_written_to_state_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();

        let config = GlobalConfig {
            wan_iface: Some("eth0".to_string()),
            state_dir: dir.path().to_path_buf(),
            ..zfs_config("testpool", "ember")
        };
        store.write(&store.config_path(), &config).unwrap();

        let loaded: GlobalConfig = store.read(&store.config_path()).unwrap();
        assert_eq!(loaded.pool, "testpool");
        assert_eq!(loaded.dataset, "ember");
        assert_eq!(loaded.kernel_path, None);
        assert_eq!(loaded.wan_iface, Some("eth0".to_string()));
    }

    #[test]
    fn global_config_overwritten_on_reinit() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();

        let config1 = GlobalConfig {
            wan_iface: Some("eth0".to_string()),
            state_dir: dir.path().to_path_buf(),
            ..zfs_config("pool1", "ds1")
        };
        store.write(&store.config_path(), &config1).unwrap();

        let config2 = GlobalConfig {
            kernel_path: Some(PathBuf::from("/kernels/vmlinux")),
            wan_iface: Some("wlp2s0".to_string()),
            state_dir: dir.path().to_path_buf(),
            ..zfs_config("pool2", "ds2")
        };
        store.write(&store.config_path(), &config2).unwrap();

        let loaded: GlobalConfig = store.read(&store.config_path()).unwrap();
        assert_eq!(loaded, config2);
    }

    #[test]
    fn global_config_backwards_compatible_without_wan_iface() {
        // Older config.json files won't have wan_iface or storage_backend
        // — serde(default) handles both.
        let json = r#"{"pool":"tank","dataset":"ember","kernel_path":null}"#;
        let loaded: GlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.pool, "tank");
        assert_eq!(loaded.wan_iface, None);
        assert_eq!(loaded.storage_backend, StorageKind::Zfs);
    }
}
