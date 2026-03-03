//! Named kernel presets for Firecracker microVMs.
//!
//! Provides [`KernelPreset`] (known kernels with download URLs) and
//! [`KernelSpec`] (either a preset name or an explicit file path).
//! Both the CLI flags and YAML config parse into `KernelSpec`.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::de;

use crate::state::store::StateStore;

/// Named kernel presets with known download URLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelPreset {
    /// Firecracker CI kernel (vmlinux-6.1.102). Includes overlayfs,
    /// cgroups, namespaces, iptables, bridge, veth, and virtio-rng.
    Stock,
}

/// The default kernel preset used when no kernel is specified.
pub const DEFAULT_PRESET: KernelPreset = KernelPreset::Stock;

impl KernelPreset {
    /// Download URL for this preset on the current architecture.
    pub fn url(&self) -> String {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "aarch64",
            _ => "x86_64",
        };
        match self {
            KernelPreset::Stock => format!(
                "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.11/{arch}/vmlinux-6.1.102"
            ),
        }
    }

    /// Filename used when saving this kernel to the kernels/ directory.
    pub fn filename(&self) -> &'static str {
        match self {
            KernelPreset::Stock => "vmlinux-6.1.102",
        }
    }
}

impl fmt::Display for KernelPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelPreset::Stock => write!(f, "stock"),
        }
    }
}

impl FromStr for KernelPreset {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "stock" => Ok(KernelPreset::Stock),
            _ => Err(()),
        }
    }
}

/// A kernel specification: either a named preset or an explicit file path.
///
/// When parsed from a string (CLI flag or YAML config), known preset names
/// (`stock`) are recognized; anything else is treated as a filesystem path.
#[derive(Debug, Clone, PartialEq)]
pub enum KernelSpec {
    Preset(KernelPreset),
    Path(PathBuf),
}

impl KernelSpec {
    /// Resolve this kernel spec to a concrete filesystem path.
    ///
    /// For presets, downloads the kernel to the state store's `kernels/`
    /// directory if not already cached. For paths, applies tilde expansion
    /// and returns the path as-is.
    pub fn resolve(&self, store: &StateStore) -> anyhow::Result<PathBuf> {
        match self {
            KernelSpec::Path(p) => Ok(crate::config::vm::expand_tilde(p)),
            KernelSpec::Preset(preset) => {
                let dest = store.kernel_dir().join(preset.filename());
                if dest.exists() {
                    println!("Using {preset} kernel at {}", dest.display());
                } else {
                    let url = preset.url();
                    println!("Downloading {preset} kernel from {url}...");
                    crate::cli::init::download_file(&url, &dest)?;
                    println!("Kernel saved to {}", dest.display());
                }
                Ok(dest)
            }
        }
    }
}

impl FromStr for KernelSpec {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
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
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
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
    fn parse_preset_case_insensitive() {
        assert_eq!("STOCK".parse::<KernelPreset>(), Ok(KernelPreset::Stock));
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
        let expected_arch = if arch == "aarch64" { "aarch64" } else { "x86_64" };
        assert!(KernelPreset::Stock.url().contains(expected_arch));
    }

    #[test]
    fn display_round_trip() {
        assert_eq!(KernelPreset::Stock.to_string(), "stock");
    }

    #[test]
    fn serde_deserialize_preset() {
        let spec: KernelSpec = serde_json::from_str(r#""stock""#).unwrap();
        assert_eq!(spec, KernelSpec::Preset(KernelPreset::Stock));
    }

    #[test]
    fn serde_deserialize_path() {
        let spec: KernelSpec = serde_json::from_str(r#""/path/to/vmlinux""#).unwrap();
        assert_eq!(spec, KernelSpec::Path(PathBuf::from("/path/to/vmlinux")));
    }
}
