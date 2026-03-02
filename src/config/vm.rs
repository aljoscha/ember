//! YAML configuration file for VM creation.
//!
//! Provides types for deserializing per-VM YAML config files passed
//! via `--vm-config`. All fields are optional — they override program
//! defaults and are in turn overridden by explicit CLI flags.
//!
//! Merge order: program defaults < YAML config < CLI flags.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::size::ByteSize;
use crate::error::Error;

/// Per-VM YAML configuration.
///
/// All fields are optional — present values override program defaults.
/// CLI flags in turn override YAML values.
#[derive(Debug, Default, Deserialize)]
pub struct VmConfig {
    /// VM name (informational — CLI positional arg takes precedence).
    pub name: Option<String>,
    /// Base image reference (e.g., "docker.io/library/ubuntu:22.04").
    pub image: Option<String>,
    /// Number of vCPUs.
    pub cpus: Option<u32>,
    /// Memory size (e.g., `512M`, `16G`).
    pub memory: Option<ByteSize>,
    /// Disk size (e.g., `8G`, `512M`).
    pub disk_size: Option<ByteSize>,
    /// Path to custom kernel.
    pub kernel: Option<PathBuf>,
    /// Network configuration.
    pub network: Option<VmNetworkConfig>,
    /// SSH configuration.
    pub ssh: Option<VmSshConfig>,
    /// Custom boot arguments for the kernel.
    pub boot_args: Option<String>,
}

/// Network configuration within a VM YAML config.
#[derive(Debug, Deserialize)]
pub struct VmNetworkConfig {
    /// Subnet for IP allocation (e.g., "10.100.0.0/16").
    pub subnet: Option<String>,
}

/// SSH configuration within a VM YAML config.
#[derive(Debug, Deserialize)]
pub struct VmSshConfig {
    /// SSH user to connect as.
    pub user: Option<String>,
    /// Path to SSH private key.
    pub key: Option<PathBuf>,
}

/// Load a VM config from a YAML file.
pub fn load(path: &Path) -> crate::error::Result<VmConfig> {
    let contents = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    let config: VmConfig = serde_yaml::from_str(&contents)?;
    Ok(config)
}

/// Expand `~/...` to the user's home directory.
///
/// Paths in YAML config files aren't shell-expanded, so a literal
/// `~/` prefix needs manual expansion.
pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let yaml = r#"
name: myvm
image: docker.io/library/ubuntu:22.04
cpus: 2
memory: 512M
disk_size: 4G
kernel: /path/to/vmlinux
network:
  subnet: 10.100.0.0/16
ssh:
  user: root
  key: /root/.ssh/id_ed25519
boot_args: "console=ttyS0 reboot=k panic=1 pci=off"
"#;
        let config: VmConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name.as_deref(), Some("myvm"));
        assert_eq!(
            config.image.as_deref(),
            Some("docker.io/library/ubuntu:22.04")
        );
        assert_eq!(config.cpus, Some(2));
        assert_eq!(config.memory.unwrap().to_mib().unwrap(), 512);
        assert_eq!(config.disk_size.unwrap().to_gib().unwrap(), 4);
        assert_eq!(config.kernel, Some(PathBuf::from("/path/to/vmlinux")));
        assert_eq!(
            config.network.as_ref().unwrap().subnet.as_deref(),
            Some("10.100.0.0/16")
        );
        assert_eq!(config.ssh.as_ref().unwrap().user.as_deref(), Some("root"));
        assert_eq!(
            config.ssh.as_ref().unwrap().key,
            Some(PathBuf::from("/root/.ssh/id_ed25519"))
        );
        assert_eq!(
            config.boot_args.as_deref(),
            Some("console=ttyS0 reboot=k panic=1 pci=off")
        );
    }

    #[test]
    fn reject_bare_integer_memory() {
        let yaml = "memory: 512\n";
        let err = serde_yaml::from_str::<VmConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("requires a unit suffix"), "got: {msg}");
    }

    #[test]
    fn reject_bare_integer_disk_size() {
        let yaml = "disk_size: 4\n";
        let err = serde_yaml::from_str::<VmConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("requires a unit suffix"), "got: {msg}");
    }

    #[test]
    fn parse_minimal_config() {
        let yaml = "image: alpine:latest\n";
        let config: VmConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.image.as_deref(), Some("alpine:latest"));
        assert!(config.name.is_none());
        assert!(config.cpus.is_none());
        assert!(config.memory.is_none());
        assert!(config.disk_size.is_none());
        assert!(config.kernel.is_none());
        assert!(config.network.is_none());
        assert!(config.ssh.is_none());
        assert!(config.boot_args.is_none());
    }

    #[test]
    fn parse_empty_config() {
        let yaml = "---\n";
        let config: VmConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.image.is_none());
        assert!(config.cpus.is_none());
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.yaml");
        std::fs::write(&path, "image: alpine:latest\ncpus: 4\n").unwrap();

        let config = load(&path).unwrap();
        assert_eq!(config.image.as_deref(), Some("alpine:latest"));
        assert_eq!(config.cpus, Some(4));
    }

    #[test]
    fn load_missing_file() {
        let result = load(Path::new("/nonexistent/config.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn load_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "cpus: not_a_number\n").unwrap();

        let result = load(&path);
        assert!(result.is_err());
    }

    #[test]
    fn expand_tilde_with_home() {
        if std::env::var_os("HOME").is_some() {
            let expanded = expand_tilde(Path::new("~/foo/bar"));
            assert!(!expanded.starts_with("~"));
            assert!(expanded.ends_with("foo/bar"));
        }
    }

    #[test]
    fn expand_tilde_absolute_path_unchanged() {
        let path = PathBuf::from("/absolute/path");
        assert_eq!(expand_tilde(&path), path);
    }

    #[test]
    fn expand_tilde_relative_path_unchanged() {
        let path = PathBuf::from("relative/path");
        assert_eq!(expand_tilde(&path), path);
    }
}
