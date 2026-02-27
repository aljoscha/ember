use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args;
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::state::store::StateStore;
use crate::zfs;

#[derive(Args)]
pub struct InitArgs {
    /// ZFS pool name
    #[arg(long, default_value = "ember")]
    pub pool: String,

    /// Block device for pool creation
    #[arg(long)]
    pub device: Option<String>,

    /// Dataset name within the pool
    #[arg(long, default_value = "ember")]
    pub dataset: String,

    /// URL to download the kernel from
    #[arg(long)]
    pub kernel_url: Option<String>,
}

/// Global configuration written by `ember init`.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub pool: String,
    pub dataset: String,
    pub kernel_path: Option<PathBuf>,
}

pub fn run(args: &InitArgs, state_dir: &Path) -> anyhow::Result<()> {
    let pool = &args.pool;

    // 1. Create or verify ZFS pool.
    if zfs::pool::exists(pool)? {
        let info = zfs::pool::status(pool)?;
        println!("Pool '{pool}' already exists (health: {})", info.health);
    } else {
        let device = args.device.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "pool '{pool}' does not exist — provide --device to create it"
            )
        })?;
        println!("Creating ZFS pool '{pool}' on {device}...");
        zfs::pool::create(pool, device)?;
        println!("Pool '{pool}' created.");
    }

    // 2. Create datasets: <pool>/<dataset>, <pool>/<dataset>/images, <pool>/<dataset>/vms.
    let base = format!("{pool}/{}", args.dataset);
    let images = format!("{base}/images");
    let vms = format!("{base}/vms");

    for ds in [&base, &images, &vms] {
        if zfs::dataset::exists(ds)? {
            println!("Dataset '{ds}' already exists.");
        } else {
            println!("Creating dataset '{ds}'...");
            zfs::dataset::create(ds)?;
        }
    }

    // 3. Initialize state directory structure.
    let store = StateStore::new(state_dir.to_path_buf());
    store.init()?;
    println!("State directory initialized at {}", state_dir.display());

    // 4. Download kernel if URL provided.
    let kernel_path = if let Some(url) = &args.kernel_url {
        let filename = url.rsplit('/').next().unwrap_or("vmlinux");
        let dest = store.kernel_dir().join(filename);

        if dest.exists() {
            println!("Kernel already exists at {}", dest.display());
        } else {
            println!("Downloading kernel from {url}...");
            download_file(url, &dest)?;
            println!("Kernel saved to {}", dest.display());
        }
        Some(dest)
    } else {
        println!("No --kernel-url provided, skipping kernel download.");
        None
    };

    // 5. Write config.
    let config = GlobalConfig {
        pool: pool.clone(),
        dataset: args.dataset.clone(),
        kernel_path,
    };
    store.write(&store.config_path(), &config)?;
    println!("Configuration written to {}", store.config_path().display());

    println!("\nember initialized successfully.");
    Ok(())
}

/// Download a file using curl.
pub(super) fn download_file(url: &str, dest: &Path) -> crate::error::Result<()> {
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
        };

        let json: serde_json::Value = serde_json::to_value(&config).unwrap();
        assert_eq!(json["pool"], "tank");
        assert_eq!(json["dataset"], "ember");
        assert_eq!(json["kernel_path"], "/kernels/vmlinux");
    }

    #[test]
    fn global_config_null_kernel_in_json() {
        let config = GlobalConfig {
            pool: "tank".to_string(),
            dataset: "ember".to_string(),
            kernel_path: None,
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
        };
        store.write(&store.config_path(), &config).unwrap();

        let loaded: GlobalConfig = store.read(&store.config_path()).unwrap();
        assert_eq!(loaded.pool, "testpool");
        assert_eq!(loaded.dataset, "ember");
        assert_eq!(loaded.kernel_path, None);
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
        };
        store.write(&store.config_path(), &config1).unwrap();

        // Second write (simulates re-running init).
        let config2 = GlobalConfig {
            pool: "pool2".to_string(),
            dataset: "ds2".to_string(),
            kernel_path: Some(PathBuf::from("/kernels/vmlinux")),
        };
        store.write(&store.config_path(), &config2).unwrap();

        let loaded: GlobalConfig = store.read(&store.config_path()).unwrap();
        assert_eq!(loaded, config2);
    }
}
