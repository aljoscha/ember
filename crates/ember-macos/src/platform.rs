//! macOS platform implementation.
//!
//! Implements the [`Platform`] trait for macOS: APFS clones, vmnet networking,
//! `mkfs.ext4` from Homebrew e2fsprogs, and `gtar` for OCI layer extraction.

use std::path::{Path, PathBuf};

use ember_core::backend::{ImageToolConfig, Platform, ResolvConfMode};
use ember_core::config::GlobalConfig;
use ember_core::error::Result;
use ember_core::image::registry::ImageEntry;
use ember_core::state::vm::VmMetadata;

/// macOS platform implementation.
pub struct MacosPlatform;

/// Generate a Homebrew install hint for a missing tool.
fn macos_install_hint(name: &str) -> String {
    let pkg = match name {
        "gtar" => "gnu-tar",
        _ => name,
    };
    format!("`brew install {pkg}`")
}

impl Platform for MacosPlatform {
    const REQUIRES_ROOT: bool = false;

    fn reconcile(state_dir: &Path) {
        crate::reconcile::run(state_dir);
    }

    fn default_state_dir() -> PathBuf {
        if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join("Library/Application Support/ember")
        } else {
            PathBuf::from("/var/lib/ember")
        }
    }

    fn console_device() -> &'static str {
        "hvc0"
    }

    fn resolv_conf_mode() -> ResolvConfMode {
        ResolvConfMode::StaticContent("nameserver 8.8.8.8\nnameserver 1.1.1.1\n")
    }

    fn image_tool_config() -> ImageToolConfig {
        ImageToolConfig {
            tar_command: "gtar",
            needs_fakeroot: !nix::unistd::geteuid().is_root(),
            override_os: Some("linux"),
            install_hint: macos_install_hint,
        }
    }

    fn init_hint() -> &'static str {
        "Run: ember init"
    }

    fn inspect_vm_extra(metadata: &VmMetadata) -> Vec<(&'static str, String)> {
        vec![("Disk image", metadata.disk_path.clone())]
    }

    fn inspect_image_extra(entry: &ImageEntry) -> Vec<(&'static str, String)> {
        vec![("Disk image", entry.disk_path.clone())]
    }

    fn info_extra(_config: &GlobalConfig) -> Vec<(&'static str, String)> {
        vec![]
    }

    fn pre_pause_check(_metadata: &VmMetadata) -> anyhow::Result<()> {
        Ok(())
    }

    fn post_delete_cleanup() {}

    fn detect_wan_iface(user_provided: Option<&str>) -> (Option<String>, Vec<String>) {
        if let Some(iface) = user_provided {
            return (
                Some(iface.to_string()),
                vec![format!("Using user-provided WAN interface: {iface}")],
            );
        }

        match crate::network::detect_wan_iface() {
            Ok(iface) => {
                let msg = format!("Detected WAN interface: {iface}");
                (Some(iface), vec![msg])
            }
            Err(e) => {
                let msg = format!("Warning: could not detect WAN interface: {e}");
                (None, vec![msg])
            }
        }
    }

    fn create_ext4_image(rootfs_dir: &Path, image_path: &Path, size_mib: u64) -> Result<()> {
        crate::image::create(rootfs_dir, image_path, size_mib)
    }

    fn estimate_ext4_size_mib(rootfs_dir: &Path) -> Result<u64> {
        crate::image::estimate_size_mib(rootfs_dir)
    }
}
