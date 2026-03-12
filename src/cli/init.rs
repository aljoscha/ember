use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args;
use serde::{Deserialize, Serialize};

use crate::backend::{InitConfig, Storage, StorageBackend};
use crate::error::Error;
use crate::state::store::StateStore;

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
    pub kernel: Option<crate::kernel::KernelSpec>,

    /// WAN interface for NAT (auto-detected if not specified)
    #[arg(long)]
    pub wan_iface: Option<String>,
}

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

pub fn run(args: &InitArgs, state_dir: &Path) -> anyhow::Result<()> {
    // 1-2. Create or verify ZFS pool and datasets via the storage backend.
    let init_config = InitConfig {
        state_dir: state_dir.to_path_buf(),
        pool: args.pool.clone(),
        dataset: args.dataset.clone(),
        device: args.device.clone(),
    };
    Storage::init(&init_config)?;

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
    // On macOS, vmnet handles networking — no WAN interface detection needed.
    #[cfg(target_os = "linux")]
    let wan_iface = if let Some(iface) = &args.wan_iface {
        println!("Using WAN interface '{iface}' (from --wan-iface).");
        Some(iface.clone())
    } else {
        match crate::network::wan::detect() {
            Ok(iface) => {
                println!("Detected WAN interface: {iface}");
                Some(iface)
            }
            Err(e) => {
                println!("Warning: could not detect WAN interface: {e}");
                println!("Networking will require --wan-iface at init time.");
                None
            }
        }
    };
    #[cfg(target_os = "macos")]
    let wan_iface = if let Some(iface) = &args.wan_iface {
        println!("Using WAN interface '{iface}' (from --wan-iface).");
        Some(iface.clone())
    } else {
        match crate::backend::macos::network::detect_wan_iface() {
            Ok(iface) => {
                println!("Detected WAN interface: {iface}");
                Some(iface)
            }
            Err(e) => {
                println!("Warning: could not detect WAN interface: {e}");
                None
            }
        }
    };

    // 6. Write config.
    let config = GlobalConfig {
        pool: args.pool.clone(),
        dataset: args.dataset.clone(),
        kernel_path,
        wan_iface,
        state_dir: state_dir.to_path_buf(),
    };
    store.write(&store.config_path(), &config)?;
    println!("Configuration written to {}", store.config_path().display());

    println!("\nember initialized successfully.");
    Ok(())
}

/// Download a file using curl.
pub(crate) fn download_file(url: &str, dest: &Path) -> crate::error::Result<()> {
    let output = Command::new("curl")
        .args(["-fSL", "-o"])
        .arg(dest)
        .arg(url)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "curl".to_string(),
            source: e,
        })?;

    Error::check_command("curl", output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_config_round_trip_with_kernel() {
        let config = GlobalConfig {
            pool: "testpool".to_string(),
            dataset: "ember".to_string(),
            kernel_path: Some(PathBuf::from("/var/lib/ember/kernels/vmlinux")),
            wan_iface: Some("eth0".to_string()),
            state_dir: PathBuf::from("/var/lib/ember"),
        };

        let json = serde_json::to_string(&config).unwrap();
        let loaded: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn global_config_round_trip_without_kernel() {
        let config = GlobalConfig {
            pool: "mypool".to_string(),
            dataset: "mydata".to_string(),
            kernel_path: None,
            wan_iface: None,
            state_dir: PathBuf::default(),
        };

        let json = serde_json::to_string(&config).unwrap();
        let loaded: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn global_config_json_format() {
        let config = GlobalConfig {
            pool: "tank".to_string(),
            dataset: "ember".to_string(),
            kernel_path: Some(PathBuf::from("/kernels/vmlinux")),
            wan_iface: Some("wlp2s0".to_string()),
            state_dir: PathBuf::default(),
        };

        let json: serde_json::Value = serde_json::to_value(&config).unwrap();
        assert_eq!(json["pool"], "tank");
        assert_eq!(json["dataset"], "ember");
        assert_eq!(json["kernel_path"], "/kernels/vmlinux");
        assert_eq!(json["wan_iface"], "wlp2s0");
    }

    #[test]
    fn global_config_null_kernel_in_json() {
        let config = GlobalConfig {
            pool: "tank".to_string(),
            dataset: "ember".to_string(),
            kernel_path: None,
            wan_iface: None,
            state_dir: PathBuf::default(),
        };

        let json: serde_json::Value = serde_json::to_value(&config).unwrap();
        assert!(json["kernel_path"].is_null());
    }

    #[test]
    fn global_config_written_to_state_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();

        let config = GlobalConfig {
            pool: "testpool".to_string(),
            dataset: "ember".to_string(),
            kernel_path: None,
            wan_iface: Some("eth0".to_string()),
            state_dir: dir.path().to_path_buf(),
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

        // First write.
        let config1 = GlobalConfig {
            pool: "pool1".to_string(),
            dataset: "ds1".to_string(),
            kernel_path: None,
            wan_iface: Some("eth0".to_string()),
            state_dir: dir.path().to_path_buf(),
        };
        store.write(&store.config_path(), &config1).unwrap();

        // Second write (simulates re-running init).
        let config2 = GlobalConfig {
            pool: "pool2".to_string(),
            dataset: "ds2".to_string(),
            kernel_path: Some(PathBuf::from("/kernels/vmlinux")),
            wan_iface: Some("wlp2s0".to_string()),
            state_dir: dir.path().to_path_buf(),
        };
        store.write(&store.config_path(), &config2).unwrap();

        let loaded: GlobalConfig = store.read(&store.config_path()).unwrap();
        assert_eq!(loaded, config2);
    }

    #[test]
    fn global_config_backwards_compatible_without_wan_iface() {
        // Older config.json files won't have wan_iface — serde(default) handles this.
        let json = r#"{"pool":"tank","dataset":"ember","kernel_path":null}"#;
        let loaded: GlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.pool, "tank");
        assert_eq!(loaded.wan_iface, None);
    }
}
