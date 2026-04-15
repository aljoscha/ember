//! Linux VM backend: Firecracker process management and API control.
//!
//! Wraps the `firecracker::process` and `firecracker::api` modules behind
//! the [`VmBackend`] trait. Firecracker is spawned as a child process and
//! controlled via its Unix socket REST API.
//!
//! **Start flow**: expects networking to already be configured in
//! `vm.network` (set up by [`NetworkBackend::setup`]). Spawns Firecracker,
//! configures it via the API (CPU, memory, kernel, rootfs, network), and boots.
//!
//! **Stop flow**: attempts graceful shutdown via SSH `reboot` command
//! (which triggers a KVM_EXIT_SHUTDOWN via the `reboot=k` boot arg),
//! falls back to Firecracker `SendCtrlAltDel` API, then SIGKILL.

use std::time::Duration;

use crate::firecracker;
use crate::network;
use crate::zfs;
use ember_core::backend::{StartedVm, VmBackend};
use ember_core::config::GlobalConfig;
use ember_core::error::{Error, Result};
use ember_core::ssh;
use ember_core::state::vm::{NetworkInfo, VmMetadata};

/// Linux VM backend using Firecracker (KVM).
pub struct LinuxVm;

/// Timeout for graceful VM shutdown before falling back to SIGKILL.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for SIGKILL to take effect.
const FORCE_KILL_TIMEOUT: Duration = Duration::from_secs(5);

impl VmBackend for LinuxVm {
    /// Start a Firecracker VM.
    ///
    /// Expects `vm.network` to be already populated (by `NetworkBackend::setup`).
    /// Spawns the Firecracker process, configures it via the API, and boots.
    /// Returns the hypervisor PID and the network info from the metadata.
    fn start(vm: &VmMetadata, _config: &GlobalConfig) -> Result<StartedVm> {
        let socket_path = &vm.api_socket;
        let log_path = socket_path.with_file_name("firecracker.log");

        let net_info = vm.network.as_ref().ok_or_else(|| {
            Error::Firecracker(format!(
                "vm '{}': network must be configured before start",
                vm.name
            ))
        })?;

        // Clean up stale socket from a previous run.
        if socket_path.exists() {
            std::fs::remove_file(socket_path).map_err(|e| Error::Io {
                path: socket_path.clone(),
                source: e,
            })?;
        }

        // Spawn Firecracker process.
        let child = firecracker::process::spawn(socket_path, &log_path)
            .map_err(|e| Error::Firecracker(e.to_string()))?;
        let pid = child.id();

        // Configure and boot via the Firecracker API.
        // Kill the process on failure to avoid an orphaned Firecracker.
        match configure_and_boot(vm, socket_path, net_info) {
            Ok(()) => {}
            Err(e) => {
                let _ = firecracker::process::kill(pid);
                return Err(e);
            }
        }

        Ok(StartedVm {
            pid,
            network: net_info.clone(),
        })
    }

    /// Graceful stop: SSH reboot → SendCtrlAltDel → wait → SIGKILL fallback.
    ///
    /// Network teardown is NOT handled here — the caller should invoke
    /// `NetworkBackend::teardown` separately after stop returns.
    fn stop(vm: &VmMetadata) -> Result<()> {
        let pid = vm
            .pid
            .ok_or_else(|| Error::Firecracker(format!("vm '{}' has no PID", vm.name)))?;

        if !firecracker::process::is_alive(pid) {
            cleanup_socket(vm);
            return Ok(());
        }

        // Try graceful shutdown via SSH reboot, then SendCtrlAltDel fallback.
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Firecracker(format!("failed to create tokio runtime: {e}")))?;

        let ssh_sent = try_ssh_reboot(&rt, vm);
        if !ssh_sent {
            try_ctrl_alt_del(&rt, vm);
        }

        // Wait for exit, then SIGKILL if still alive.
        if !firecracker::process::wait_for_exit(pid, GRACEFUL_SHUTDOWN_TIMEOUT) {
            firecracker::process::kill(pid).map_err(|e| Error::Firecracker(e.to_string()))?;
            firecracker::process::wait_for_exit(pid, FORCE_KILL_TIMEOUT);
        }

        cleanup_socket(vm);
        Ok(())
    }

    /// Force stop: SIGKILL immediately.
    fn force_stop(vm: &VmMetadata) -> Result<()> {
        let pid = vm
            .pid
            .ok_or_else(|| Error::Firecracker(format!("vm '{}' has no PID", vm.name)))?;

        firecracker::process::kill(pid).map_err(|e| Error::Firecracker(e.to_string()))?;
        firecracker::process::wait_for_exit(pid, FORCE_KILL_TIMEOUT);

        cleanup_socket(vm);
        Ok(())
    }

    /// Pause via Firecracker PATCH /vm { state: "Paused" }.
    fn pause(vm: &VmMetadata) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Firecracker(format!("failed to create tokio runtime: {e}")))?;
        rt.block_on(async {
            let client = firecracker::api::FirecrackerClient::new(&vm.api_socket);
            client
                .patch_vm(&firecracker::api::VmStateUpdate::pause())
                .await
        })
        .map_err(|e| Error::Firecracker(e.to_string()))
    }

    /// Resume via Firecracker PATCH /vm { state: "Resumed" }.
    fn resume(vm: &VmMetadata) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()
            .map_err(|e| Error::Firecracker(format!("failed to create tokio runtime: {e}")))?;
        rt.block_on(async {
            let client = firecracker::api::FirecrackerClient::new(&vm.api_socket);
            client
                .patch_vm(&firecracker::api::VmStateUpdate::resume())
                .await
        })
        .map_err(|e| Error::Firecracker(e.to_string()))
    }

    /// Check whether the Firecracker process is alive via `kill(pid, 0)`.
    fn is_running(pid: u32) -> bool {
        firecracker::process::is_alive(pid)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Configure and boot a Firecracker VM via the REST API.
///
/// Waits for the API socket, builds the VM configuration from metadata
/// and network info, then issues the API calls to configure and start
/// the instance.
fn configure_and_boot(
    vm: &VmMetadata,
    socket_path: &std::path::Path,
    net_info: &NetworkInfo,
) -> Result<()> {
    firecracker::process::wait_for_socket(socket_path)
        .map_err(|e| Error::Firecracker(e.to_string()))?;

    // Detect host DNS servers for the guest, scoped to the WAN interface
    // so we only get servers reachable through the VM's NAT path.
    let wan_iface = net_info.wan_iface.as_deref().unwrap_or("eth0");
    let dns_servers = network::dns::detect_nameservers(wan_iface);

    // Build VM configuration.
    let rootfs_path = zfs::volume::device_path(&vm.disk_path);
    let mut vm_config =
        firecracker::config::VmConfig::new(vm.cpus, vm.memory_mib, &vm.kernel_path, &rootfs_path);
    if let Some(ref boot_args) = vm.boot_args {
        vm_config = vm_config.with_boot_args(boot_args);
    }
    let mut vm_config = vm_config.with_network(firecracker::config::VmNetworkConfig {
        tap_device: net_info.tap_device.clone(),
        guest_ip: net_info.guest_ip.clone(),
        host_ip: net_info.host_ip.clone(),
        netmask: net_info.netmask.clone(),
        guest_mac: net_info.guest_mac.clone(),
        hostname: vm.name.clone(),
        dns_servers,
    });

    // Configure vsock device if enabled.
    if let Some(ref vsock) = vm.vsock {
        vm_config = vm_config.with_vsock(
            vsock.uds_path.to_string_lossy().to_string(),
            vsock.guest_cid,
        );
    }

    // Run the async API calls in a blocking runtime.
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| Error::Firecracker(format!("failed to create tokio runtime: {e}")))?;
    rt.block_on(async {
        let client = firecracker::api::FirecrackerClient::new(socket_path);
        vm_config.configure_and_start(&client).await
    })
    .map_err(|e| Error::Firecracker(e.to_string()))
}

/// Attempt graceful shutdown via SSH `reboot` command.
///
/// Uses `reboot` (not `poweroff`) because Firecracker has no ACPI — the
/// `reboot=k` boot arg causes the kernel to trigger a CPU reset, which
/// KVM reports as KVM_EXIT_SHUTDOWN, allowing Firecracker to exit cleanly.
///
/// Returns `true` if the reboot command was sent successfully.
fn try_ssh_reboot(rt: &tokio::runtime::Runtime, vm: &VmMetadata) -> bool {
    let network = match &vm.network {
        Some(net) => net,
        None => return false,
    };

    rt.block_on(async {
        let timeout = Duration::from_secs(3);
        let mut client = match ssh::client::connect_with_timeout(
            &network.guest_ip,
            &vm.ssh.user,
            &vm.ssh.key,
            timeout,
        )
        .await
        {
            Ok(c) => c,
            Err(_) => return false,
        };

        // Send reboot; ignore exec errors since the connection drops
        // as the VM goes down.
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            ssh::exec::exec(&mut client, "sudo reboot"),
        )
        .await;

        true
    })
}

/// Attempt graceful shutdown via Firecracker SendCtrlAltDel API.
fn try_ctrl_alt_del(rt: &tokio::runtime::Runtime, vm: &VmMetadata) {
    if !vm.api_socket.exists() {
        return;
    }

    let _ = rt.block_on(async {
        let client = firecracker::api::FirecrackerClient::new(&vm.api_socket);
        client
            .put_action(&firecracker::api::InstanceAction::send_ctrl_alt_del())
            .await
    });
}

/// Remove the Firecracker API socket file if it exists.
fn cleanup_socket(vm: &VmMetadata) {
    if vm.api_socket.exists() {
        let _ = std::fs::remove_file(&vm.api_socket);
    }
}
