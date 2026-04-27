use std::path::Path;

use clap::Args;

use crate::backend::{init_storage, CurrentPlatform, InitConfig, Platform};
use ember_core::config::GlobalConfig;
use ember_core::state::store::StateStore;

#[derive(Args)]
pub struct InitArgs {
    /// ZFS pool name (Linux only)
    #[cfg_attr(target_os = "macos", arg(long, default_value = "ember", hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long, default_value = "ember"))]
    pub pool: String,

    /// Block device for pool creation (Linux only)
    #[cfg_attr(target_os = "macos", arg(long, hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long))]
    pub device: Option<String>,

    /// Dataset name within the pool (Linux only)
    #[cfg_attr(target_os = "macos", arg(long, default_value = "ember", hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long, default_value = "ember"))]
    pub dataset: String,

    /// Kernel preset or file path [presets: stock]
    #[arg(long)]
    pub kernel: Option<ember_core::kernel::KernelSpec>,

    /// WAN interface for NAT (auto-detected if not specified)
    #[arg(long)]
    pub wan_iface: Option<String>,
}

pub fn run(args: &InitArgs, state_dir: &Path) -> anyhow::Result<()> {
    // 1-2. Create or verify ZFS pool and datasets via the storage backend.
    let init_config = InitConfig {
        state_dir: state_dir.to_path_buf(),
        pool: args.pool.clone(),
        dataset: args.dataset.clone(),
        device: args.device.clone(),
        storage_path: None,
        btrfs_size: None,
        dm_thin_size: None,
        dm_thin_metadata_size: None,
        dm_thin_block_size: None,
    };
    init_storage(&init_config)?;

    // 3. Initialize state directory structure.
    let store = StateStore::new(state_dir.to_path_buf());
    store.init()?;
    println!("State directory initialized at {}", state_dir.display());

    // 4. Download kernel if preset or path provided.
    let kernel_path = if let Some(spec) = &args.kernel {
        Some(spec.resolve(&store)?)
    } else {
        println!("No --kernel provided; a default kernel will be downloaded on first 'vm create'.");
        None
    };

    // 5. Detect or use provided WAN interface.
    let (wan_iface, messages) = CurrentPlatform::detect_wan_iface(args.wan_iface.as_deref());
    for msg in &messages {
        println!("{msg}");
    }

    // 6. Write config.
    let config = GlobalConfig {
        storage_backend: ember_core::config::StorageKind::Zfs,
        pool: args.pool.clone(),
        dataset: args.dataset.clone(),
        kernel_path,
        wan_iface,
        state_dir: state_dir.to_path_buf(),
        storage_path: None,
        dm_thin_block_size: None,
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
