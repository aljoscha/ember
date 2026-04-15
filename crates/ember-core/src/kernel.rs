//! Named kernel presets for Firecracker microVMs.
//!
//! Provides [`KernelPreset`] (known kernels with download URLs) and
//! [`KernelSpec`] (either a preset name or an explicit file path).
//! Both the CLI flags and YAML config parse into `KernelSpec`.

pub mod build;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use serde::de;

use crate::error::{Error, Result};
use crate::state::store::StateStore;

/// Download a file using curl.
pub fn download_file(url: &str, dest: &Path) -> Result<()> {
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

/// Named kernel presets with known download URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelPreset {
    /// Firecracker CI kernel (vmlinux-6.1.102). Includes overlayfs,
    /// cgroups, namespaces, iptables, bridge, veth, and virtio-rng.
    Stock,

    /// Custom-built kernel based on Amazon Linux 6.1.163 with Docker
    /// networking support (iptables raw, nftables, dummy interface).
    /// Built locally via `ember kernel build`.
    Docker,
}

/// The default kernel preset used when no kernel is specified.
pub const DEFAULT_PRESET: KernelPreset = KernelPreset::Docker;

impl KernelPreset {
    /// Download URL for this preset on the current architecture.
    ///
    /// Returns `None` for presets that must be built locally.
    pub fn url(&self) -> Option<String> {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "aarch64",
            _ => "x86_64",
        };
        match self {
            KernelPreset::Stock => Some(format!(
                "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/{arch}/vmlinux-6.1.102"
            )),
            KernelPreset::Docker => None,
        }
    }

    /// Filename used when saving this kernel to the kernels/ directory.
    pub fn filename(&self) -> &'static str {
        match self {
            KernelPreset::Stock => "vmlinux-6.1.102",
            KernelPreset::Docker => "vmlinux-docker-6.1.163",
        }
    }
}

impl fmt::Display for KernelPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelPreset::Stock => write!(f, "stock"),
            KernelPreset::Docker => write!(f, "docker"),
        }
    }
}

impl FromStr for KernelPreset {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "stock" => Ok(KernelPreset::Stock),
            "docker" => Ok(KernelPreset::Docker),
            _ => Err(()),
        }
    }
}

/// A kernel specification: either a named preset or an explicit file path.
///
/// When parsed from a string (CLI flag or YAML config), known preset names
/// (`stock`, `docker`) are recognized; anything else is treated as a filesystem path.
#[derive(Debug, Clone, PartialEq)]
pub enum KernelSpec {
    Preset(KernelPreset),
    Path(PathBuf),
}

impl KernelSpec {
    /// Resolve this kernel spec to a concrete filesystem path.
    ///
    /// For downloadable presets, downloads the kernel to the state store's
    /// `kernels/` directory if not already cached. For locally-built presets,
    /// checks that the kernel exists and returns a helpful error if not.
    /// For paths, applies tilde expansion and returns the path as-is.
    pub fn resolve(&self, store: &StateStore) -> anyhow::Result<PathBuf> {
        match self {
            KernelSpec::Path(p) => {
                let src = crate::config::vm::expand_tilde(p);
                let src = std::fs::canonicalize(&src)
                    .map_err(|e| anyhow::anyhow!("kernel path '{}': {e}", src.display()))?;
                let filename = src
                    .file_name()
                    .ok_or_else(|| anyhow::anyhow!("kernel path has no filename"))?;
                let dest = store.kernel_dir().join(filename);
                std::fs::copy(&src, &dest).map_err(|e| {
                    anyhow::anyhow!(
                        "failed to copy kernel '{}' → '{}': {e}",
                        src.display(),
                        dest.display()
                    )
                })?;
                println!("Copied kernel {} → {}", src.display(), dest.display());
                Ok(dest)
            }
            KernelSpec::Preset(preset) => {
                let dest = store.kernel_dir().join(preset.filename());
                if dest.exists() {
                    println!("Using {preset} kernel at {}", dest.display());
                    return Ok(dest);
                }
                match preset.url() {
                    Some(url) => {
                        println!("Downloading {preset} kernel from {url}...");
                        download_file(&url, &dest)?;
                        println!("Kernel saved to {}", dest.display());
                        Ok(dest)
                    }
                    None => {
                        anyhow::bail!(
                            "Default kernel has not been built yet.\n\
                             Hint: run `sudo ember kernel build` to compile a kernel \
                             with Docker networking support,\n\
                             or use `--kernel stock` for a pre-built kernel without \
                             Docker support."
                        );
                    }
                }
            }
        }
    }
}

impl FromStr for KernelSpec {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if let Ok(preset) = s.parse::<KernelPreset>() {
            Ok(KernelSpec::Preset(preset))
        } else {
            Ok(KernelSpec::Path(PathBuf::from(s)))
        }
    }
}

impl fmt::Display for KernelSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelSpec::Preset(p) => write!(f, "{p}"),
            KernelSpec::Path(p) => write!(f, "{}", p.display()),
        }
    }
}

impl<'de> de::Deserialize<'de> for KernelSpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(s.parse().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_preset_stock() {
        assert_eq!("stock".parse::<KernelPreset>(), Ok(KernelPreset::Stock));
    }

    #[test]
    fn parse_preset_docker() {
        assert_eq!("docker".parse::<KernelPreset>(), Ok(KernelPreset::Docker));
    }

    #[test]
    fn parse_preset_case_insensitive() {
        assert_eq!("STOCK".parse::<KernelPreset>(), Ok(KernelPreset::Stock));
        assert_eq!("DOCKER".parse::<KernelPreset>(), Ok(KernelPreset::Docker));
    }

    #[test]
    fn parse_preset_unknown() {
        assert!("custom".parse::<KernelPreset>().is_err());
        assert!("/path/to/vmlinux".parse::<KernelPreset>().is_err());
        assert!("containerd".parse::<KernelPreset>().is_err());
    }

    #[test]
    fn spec_from_preset_name() {
        assert_eq!(
            "stock".parse::<KernelSpec>().unwrap(),
            KernelSpec::Preset(KernelPreset::Stock)
        );
        assert_eq!(
            "docker".parse::<KernelSpec>().unwrap(),
            KernelSpec::Preset(KernelPreset::Docker)
        );
    }

    #[test]
    fn spec_from_path() {
        assert_eq!(
            "/path/to/vmlinux".parse::<KernelSpec>().unwrap(),
            KernelSpec::Path(PathBuf::from("/path/to/vmlinux"))
        );
        assert_eq!(
            "~/kernels/vmlinux".parse::<KernelSpec>().unwrap(),
            KernelSpec::Path(PathBuf::from("~/kernels/vmlinux"))
        );
    }

    #[test]
    fn containerd_is_treated_as_path() {
        // "containerd" is no longer a preset — it should parse as a path.
        assert_eq!(
            "containerd".parse::<KernelSpec>().unwrap(),
            KernelSpec::Path(PathBuf::from("containerd"))
        );
    }

    #[test]
    fn preset_urls_contain_arch() {
        let arch = std::env::consts::ARCH;
        let expected_arch = if arch == "aarch64" {
            "aarch64"
        } else {
            "x86_64"
        };
        assert!(KernelPreset::Stock.url().unwrap().contains(expected_arch));
    }

    #[test]
    fn docker_preset_has_no_url() {
        assert!(KernelPreset::Docker.url().is_none());
    }

    #[test]
    fn default_preset_is_docker() {
        assert_eq!(DEFAULT_PRESET, KernelPreset::Docker);
    }

    #[test]
    fn display_round_trip() {
        assert_eq!(KernelPreset::Stock.to_string(), "stock");
        assert_eq!(KernelPreset::Docker.to_string(), "docker");
    }

    #[test]
    fn serde_deserialize_preset() {
        let spec: KernelSpec = serde_json::from_str(r#""stock""#).unwrap();
        assert_eq!(spec, KernelSpec::Preset(KernelPreset::Stock));

        let spec: KernelSpec = serde_json::from_str(r#""docker""#).unwrap();
        assert_eq!(spec, KernelSpec::Preset(KernelPreset::Docker));
    }

    #[test]
    fn serde_deserialize_path() {
        let spec: KernelSpec = serde_json::from_str(r#""/path/to/vmlinux""#).unwrap();
        assert_eq!(spec, KernelSpec::Path(PathBuf::from("/path/to/vmlinux")));
    }
}
