//! VM configuration builder — translates user config into Firecracker API calls.
//!
//! Collects all the pieces needed to configure a Firecracker microVM
//! (CPU, memory, kernel, rootfs, optional network) and issues the
//! corresponding API calls in the correct order.

use std::path::PathBuf;

use crate::firecracker::api::{
    BootSource, Drive, FirecrackerClient, InstanceAction, MachineConfig, NetworkInterface,
};

/// Default boot arguments for the guest kernel.
pub const DEFAULT_BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off";

/// Network interface configuration for the VM.
#[derive(Debug, Clone)]
pub struct VmNetworkConfig {
    /// TAP device name on the host (e.g., "em-abc123").
    pub tap_device: String,
    /// Guest IP address (e.g., "10.100.0.2").
    pub guest_ip: String,
    /// Gateway IP address (e.g., "10.100.0.1").
    pub gateway_ip: String,
    /// Netmask (e.g., "255.255.255.252").
    pub netmask: String,
    /// Optional guest MAC address.
    pub guest_mac: Option<String>,
}

/// Collected configuration for a Firecracker microVM.
///
/// Built up incrementally, then applied as a sequence of API calls
/// to a running Firecracker process.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Number of vCPUs.
    pub vcpu_count: u32,
    /// Memory in MiB.
    pub mem_size_mib: u32,
    /// Path to the kernel image (vmlinux).
    pub kernel_image_path: PathBuf,
    /// Boot arguments for the kernel.
    pub boot_args: String,
    /// Path to the root drive (ZFS zvol block device).
    pub rootfs_path: PathBuf,
    /// Optional network interface configuration.
    pub network: Option<VmNetworkConfig>,
}

impl VmConfig {
    /// Create a new VM configuration with required parameters.
    ///
    /// Uses [`DEFAULT_BOOT_ARGS`] by default. Call [`with_boot_args`](Self::with_boot_args)
    /// to override.
    pub fn new(
        vcpu_count: u32,
        mem_size_mib: u32,
        kernel_image_path: impl Into<PathBuf>,
        rootfs_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            vcpu_count,
            mem_size_mib,
            kernel_image_path: kernel_image_path.into(),
            boot_args: DEFAULT_BOOT_ARGS.to_string(),
            rootfs_path: rootfs_path.into(),
            network: None,
        }
    }

    /// Set custom boot arguments, replacing the defaults.
    pub fn with_boot_args(mut self, args: impl Into<String>) -> Self {
        self.boot_args = args.into();
        self
    }

    /// Configure networking for the VM.
    pub fn with_network(mut self, network: VmNetworkConfig) -> Self {
        self.network = Some(network);
        self
    }

    /// Build the full boot_args string.
    ///
    /// If networking is configured, appends the kernel `ip=` parameter
    /// so the guest configures its network interface at boot without
    /// needing cloud-init or DHCP.
    fn full_boot_args(&self) -> String {
        match &self.network {
            Some(net) => format!(
                "{} ip={}::{}:{}::eth0:off",
                self.boot_args, net.guest_ip, net.gateway_ip, net.netmask
            ),
            None => self.boot_args.clone(),
        }
    }

    /// Configure a Firecracker instance via the API client.
    ///
    /// Issues the following API calls in order:
    /// 1. `PUT /machine-config` — vCPUs and memory
    /// 2. `PUT /boot-source` — kernel path and boot arguments
    /// 3. `PUT /drives/rootfs` — root block device
    /// 4. `PUT /network-interfaces/eth0` — TAP device (if networking configured)
    ///
    /// After this returns, the VM is fully configured and ready to start
    /// via [`FirecrackerClient::put_action`] with [`InstanceAction::instance_start`].
    pub async fn configure(&self, client: &FirecrackerClient) -> anyhow::Result<()> {
        // 1. Machine configuration
        client
            .put_machine_config(&MachineConfig {
                vcpu_count: self.vcpu_count,
                mem_size_mib: self.mem_size_mib,
                smt: None,
                track_dirty_pages: None,
            })
            .await?;

        // 2. Boot source
        client
            .put_boot_source(&BootSource {
                kernel_image_path: self.kernel_image_path.to_string_lossy().into_owned(),
                boot_args: Some(self.full_boot_args()),
                initrd_path: None,
            })
            .await?;

        // 3. Root drive
        client
            .put_drive(&Drive {
                drive_id: "rootfs".to_string(),
                path_on_host: self.rootfs_path.to_string_lossy().into_owned(),
                is_root_device: true,
                is_read_only: false,
            })
            .await?;

        // 4. Network interface (if configured)
        if let Some(net) = &self.network {
            client
                .put_network_interface(&NetworkInterface {
                    iface_id: "eth0".to_string(),
                    host_dev_name: net.tap_device.clone(),
                    guest_mac: net.guest_mac.clone(),
                })
                .await?;
        }

        Ok(())
    }

    /// Configure and start a Firecracker instance.
    ///
    /// Convenience method that calls [`configure`](Self::configure) followed by
    /// `PUT /actions { action_type: "InstanceStart" }`.
    pub async fn configure_and_start(&self, client: &FirecrackerClient) -> anyhow::Result<()> {
        self.configure(client).await?;
        client.put_action(&InstanceAction::instance_start()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_args_without_network() {
        let config = VmConfig::new(2, 512, "/boot/vmlinux", "/dev/zvol/pool/vms/test");
        assert_eq!(config.full_boot_args(), DEFAULT_BOOT_ARGS);
    }

    #[test]
    fn boot_args_with_network() {
        let config = VmConfig::new(2, 512, "/boot/vmlinux", "/dev/zvol/pool/vms/test")
            .with_network(VmNetworkConfig {
                tap_device: "em-abc123".to_string(),
                guest_ip: "10.100.0.2".to_string(),
                gateway_ip: "10.100.0.1".to_string(),
                netmask: "255.255.255.252".to_string(),
                guest_mac: None,
            });
        assert_eq!(
            config.full_boot_args(),
            "console=ttyS0 reboot=k panic=1 pci=off ip=10.100.0.2::10.100.0.1:255.255.255.252::eth0:off"
        );
    }

    #[test]
    fn custom_boot_args() {
        let config = VmConfig::new(1, 128, "/boot/vmlinux", "/dev/zvol/pool/vms/test")
            .with_boot_args("console=ttyS0 panic=1");
        assert_eq!(config.full_boot_args(), "console=ttyS0 panic=1");
    }

    #[test]
    fn custom_boot_args_with_network() {
        let config = VmConfig::new(1, 128, "/boot/vmlinux", "/dev/zvol/pool/vms/test")
            .with_boot_args("console=ttyS0 panic=1")
            .with_network(VmNetworkConfig {
                tap_device: "em-xyz".to_string(),
                guest_ip: "10.100.0.6".to_string(),
                gateway_ip: "10.100.0.5".to_string(),
                netmask: "255.255.255.252".to_string(),
                guest_mac: Some("AA:FC:00:00:00:01".to_string()),
            });
        assert_eq!(
            config.full_boot_args(),
            "console=ttyS0 panic=1 ip=10.100.0.6::10.100.0.5:255.255.255.252::eth0:off"
        );
    }
}
