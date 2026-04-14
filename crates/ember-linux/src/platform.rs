use std::path::{Path, PathBuf};

use ember_core::backend::{ImageToolConfig, Platform, ResolvConfMode};
use ember_core::config::GlobalConfig;
use ember_core::error::Result;
use ember_core::image::registry::ImageEntry;
use ember_core::state::vm::VmMetadata;

pub struct LinuxPlatform;

fn linux_install_hint(name: &str) -> String {
    format!("`pacman -S {name}` or `apt install {name}`")
}

impl Platform for LinuxPlatform {
    const REQUIRES_ROOT: bool = true;

    fn reconcile(state_dir: &Path) {
        crate::reconcile::run(state_dir);
    }

    fn default_state_dir() -> PathBuf {
        PathBuf::from("/var/lib/ember")
    }

    fn console_device() -> &'static str {
        "ttyS0"
    }

    fn resolv_conf_mode() -> ResolvConfMode {
        ResolvConfMode::Symlink("/proc/net/pnp")
    }

    fn image_tool_config() -> ImageToolConfig {
        ImageToolConfig {
            tar_command: "tar",
            needs_fakeroot: false,
            override_os: None,
            install_hint: linux_install_hint,
        }
    }

    fn init_hint() -> &'static str {
        "Run: ember init --pool <pool> --device <device>"
    }

    fn inspect_vm_extra(metadata: &VmMetadata) -> Vec<(&'static str, String)> {
        let mut extra = vec![
            ("ZFS zvol", metadata.disk_path.clone()),
            ("API socket", metadata.api_socket.display().to_string()),
        ];
        if let Some(ref net) = metadata.network {
            extra.push(("TAP device", net.tap_device.clone()));
        }
        extra
    }

    fn inspect_image_extra(entry: &ImageEntry) -> Vec<(&'static str, String)> {
        vec![("ZFS zvol", entry.disk_path.clone())]
    }

    fn info_extra(config: &GlobalConfig) -> Vec<(&'static str, String)> {
        let mut extra = vec![
            ("ZFS pool", config.pool.clone()),
            ("Dataset", format!("{}/{}", config.pool, config.dataset)),
        ];
        if let Some(ref wan_iface) = config.wan_iface {
            extra.push(("WAN iface", wan_iface.clone()));
        }
        extra
    }

    fn pre_pause_check(metadata: &VmMetadata) -> anyhow::Result<()> {
        let socket_path = &metadata.api_socket;
        if !socket_path.exists() {
            anyhow::bail!(
                "vm '{}' is marked as running but API socket not found at {}\n\
                 Hint: the Firecracker process may have crashed — try 'ember vm stop --force {}' and restart",
                metadata.name,
                socket_path.display(),
                metadata.name
            );
        }
        Ok(())
    }

    fn post_delete_cleanup() {
        let _ = std::process::Command::new("udevadm").arg("settle").status();
    }

    fn detect_wan_iface(user_provided: Option<&str>) -> (Option<String>, Vec<String>) {
        if let Some(iface) = user_provided {
            return (
                Some(iface.to_string()),
                vec![format!("Using WAN interface '{iface}' (from --wan-iface).")],
            );
        }
        match crate::network::wan::detect() {
            Ok(iface) => {
                let msg = format!("Detected WAN interface: {iface}");
                (Some(iface), vec![msg])
            }
            Err(e) => (
                None,
                vec![
                    format!("Warning: could not detect WAN interface: {e}"),
                    "Networking will require --wan-iface at init time.".to_string(),
                ],
            ),
        }
    }

    fn create_ext4_image(rootfs_dir: &Path, image_path: &Path, size_mib: u64) -> Result<()> {
        crate::image::create(rootfs_dir, image_path, size_mib)
    }

    fn estimate_ext4_size_mib(rootfs_dir: &Path) -> Result<u64> {
        crate::image::estimate_size_mib(rootfs_dir)
    }
}
