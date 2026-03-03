use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use clap::{Args, Subcommand};
use uuid::Uuid;

use super::init::GlobalConfig;
use crate::config;
use crate::config::size::ByteSize;
use crate::error::Error;
use crate::firecracker;
use crate::image;
use crate::image::registry::ImageRegistry;
use crate::network;
use crate::state::store::StateStore;
use crate::state::vm::{self, NetworkInfo, SshConfig, VmMetadata, VmStatus};
use crate::zfs;

#[derive(Subcommand)]
pub enum VmCommand {
    /// Create a new VM from an image
    Create(CreateArgs),

    /// Start a stopped VM
    Start(StartArgs),

    /// Stop a running VM
    Stop(StopArgs),

    /// Pause a running VM
    Pause(PauseArgs),

    /// Resume a paused VM
    Resume(ResumeArgs),

    /// Resize a stopped VM's disk
    Resize(ResizeArgs),

    /// Delete a VM and its resources
    Delete(DeleteArgs),

    /// List all VMs
    List(ListArgs),

    /// Show detailed VM information
    Inspect(InspectArgs),

    /// Open an SSH session to a VM
    Ssh(SshArgs),
}

#[derive(Args)]
pub struct CreateArgs {
    /// VM name
    pub name: String,

    /// Base image reference
    #[arg(long, required_unless_present = "vm_config")]
    pub image: Option<String>,

    /// Number of vCPUs (default: 1)
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory size, e.g. 512M, 16G (default: 16G)
    #[arg(long)]
    pub memory: Option<ByteSize>,

    /// Disk size, e.g. 8G, 512M (default: 8G)
    #[arg(long)]
    pub disk_size: Option<ByteSize>,

    /// Kernel preset or file path [presets: stock]
    #[arg(long)]
    pub kernel: Option<crate::kernel::KernelSpec>,

    /// Network subnet
    #[arg(long)]
    pub network: Option<String>,

    /// VM config YAML file
    #[arg(long = "vm-config")]
    pub vm_config: Option<PathBuf>,

    /// Don't start the VM after creation
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Args)]
pub struct StartArgs {
    /// VM name
    pub name: String,
}

#[derive(Args)]
pub struct StopArgs {
    /// VM name
    pub name: String,

    /// Force stop (SIGKILL)
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct PauseArgs {
    /// VM name
    pub name: String,
}

#[derive(Args)]
pub struct ResumeArgs {
    /// VM name
    pub name: String,
}

#[derive(Args)]
pub struct ResizeArgs {
    /// VM name
    pub name: String,

    /// New disk size with unit, e.g. 16G (must be larger than current size)
    #[arg(long)]
    pub disk_size: ByteSize,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// VM name
    pub name: String,

    /// Force delete (kill if running)
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct ListArgs {
    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct InspectArgs {
    /// VM name
    pub name: String,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct SshArgs {
    /// VM name
    pub name: String,

    /// Command to run (everything after --)
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}

pub fn run(cmd: &VmCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        VmCommand::Create(args) => create(args, state_dir),
        VmCommand::Start(args) => start(args, state_dir),
        VmCommand::Stop(args) => stop(args, state_dir),
        VmCommand::Pause(args) => pause(args, state_dir),
        VmCommand::Resume(args) => resume(args, state_dir),
        VmCommand::Resize(args) => resize(args, state_dir),
        VmCommand::Delete(args) => delete(args, state_dir),
        VmCommand::List(args) => list(args, state_dir),
        VmCommand::Inspect(args) => inspect(args, state_dir),
        VmCommand::Ssh(args) => ssh(args, state_dir),
    }
}

/// Program defaults for VM creation.
const DEFAULT_CPUS: u32 = 1;
const DEFAULT_MEMORY: ByteSize = ByteSize::from_gib(16);
const DEFAULT_DISK_SIZE: ByteSize = ByteSize::from_gib(8);

/// Resolved VM creation configuration after merging defaults, YAML config, and CLI flags.
///
/// Merge order: program defaults < YAML config < CLI flags.
#[allow(dead_code)]
struct ResolvedVmCreate {
    name: String,
    image: String,
    cpus: u32,
    memory: u32,
    disk_size: u32,
    kernel: Option<crate::kernel::KernelSpec>,
    /// Network subnet from YAML config (used during `start`, not `create`).
    network: Option<String>,
    no_start: bool,
    /// SSH user override from YAML config.
    ssh_user: Option<String>,
    /// SSH private key override from YAML config.
    ssh_key: Option<PathBuf>,
}

/// Resolve VM creation config by merging defaults, YAML config, and CLI flags.
///
/// CLI flags take highest priority, then YAML config, then program defaults.
fn resolve_create_config(
    args: &CreateArgs,
    yaml: Option<&config::vm::VmConfig>,
) -> anyhow::Result<ResolvedVmCreate> {
    let image = args
        .image
        .clone()
        .or_else(|| yaml.and_then(|c| c.image.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no image specified — provide --image or set 'image' in the YAML config"
            )
        })?;

    let cpus = args
        .cpus
        .or_else(|| yaml.and_then(|c| c.cpus))
        .unwrap_or(DEFAULT_CPUS);

    let memory_size = args
        .memory
        .or_else(|| yaml.and_then(|c| c.memory))
        .unwrap_or(DEFAULT_MEMORY);
    let memory = memory_size
        .to_mib()
        .map_err(|e| anyhow::anyhow!("invalid memory size: {e}"))?;

    let disk_size_val = args
        .disk_size
        .or_else(|| yaml.and_then(|c| c.disk_size))
        .unwrap_or(DEFAULT_DISK_SIZE);
    let disk_size = disk_size_val
        .to_gib()
        .map_err(|e| anyhow::anyhow!("invalid disk size: {e}"))?;

    let kernel = args.kernel.clone().or_else(|| yaml.and_then(|c| c.kernel.clone()));

    let network = args.network.clone().or_else(|| {
        yaml.and_then(|c| c.network.as_ref().and_then(|n| n.subnet.clone()))
    });

    let ssh_user = yaml.and_then(|c| c.ssh.as_ref().and_then(|s| s.user.clone()));
    let ssh_key = yaml.and_then(|c| {
        c.ssh
            .as_ref()
            .and_then(|s| s.key.as_ref().map(|p| config::vm::expand_tilde(p)))
    });

    Ok(ResolvedVmCreate {
        name: args.name.clone(),
        image,
        cpus,
        memory,
        disk_size,
        kernel,
        network,
        no_start: args.no_start,
        ssh_user,
        ssh_key,
    })
}

/// Resolve the kernel path: CLI/YAML spec → global config → auto-download default preset.
fn ensure_kernel(
    cli_kernel: &Option<crate::kernel::KernelSpec>,
    config: &mut GlobalConfig,
    store: &StateStore,
) -> anyhow::Result<PathBuf> {
    if let Some(spec) = cli_kernel {
        return spec.resolve(store);
    }
    if let Some(path) = &config.kernel_path {
        return Ok(path.clone());
    }

    // No kernel configured — download the default preset.
    let default_spec = crate::kernel::KernelSpec::Preset(crate::kernel::DEFAULT_PRESET);
    let dest = default_spec.resolve(store)?;

    // Persist so future creates skip the download.
    config.kernel_path = Some(dest.clone());
    store.write(&store.config_path(), config)?;

    Ok(dest)
}

/// Create a new VM from an image.
///
/// Workflow: load YAML config (if provided) → merge with CLI flags →
/// look up image → ZFS clone @base snapshot → grow zvol if needed
/// → mount zvol → inject per-VM SSH key → unmount → save metadata.
///
/// Uses a [`Rollback`] guard to ensure the zvol clone and state directory
/// are cleaned up if any step after cloning fails.
fn create(args: &CreateArgs, state_dir: &Path) -> anyhow::Result<()> {
    use crate::cleanup::Rollback;

    let store = StateStore::new(state_dir.to_path_buf());
    let mut global_config: GlobalConfig = store.read(&store.config_path())?;

    // Load YAML config if provided.
    let yaml_config = match &args.vm_config {
        Some(path) => {
            println!("Loading VM config from {}...", path.display());
            Some(config::vm::load(path)?)
        }
        None => None,
    };

    // Resolve configuration: program defaults < YAML config < CLI flags.
    let resolved = resolve_create_config(args, yaml_config.as_ref())?;

    // Check VM doesn't already exist.
    if vm::exists(&store, &resolved.name) {
        anyhow::bail!("vm '{}' already exists", resolved.name);
    }

    // Look up image in local registry.
    let registry = ImageRegistry::load(&store)?;
    let image_entry = registry
        .find_by_reference(&resolved.image)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "image '{}' not found locally — pull it first with: ember image pull {}",
                resolved.image,
                resolved.image
            )
        })?;

    let image_zvol = image_entry.zvol.clone();
    let image_ref = image_entry.reference.clone();
    let image_size_mib = image_entry.size_mib;

    // Verify @base snapshot exists on the image zvol.
    if !zfs::snapshot::exists(&image_zvol, "base")? {
        anyhow::bail!(
            "image zvol '{}' has no @base snapshot — the image may be corrupted",
            image_zvol
        );
    }

    // Build zvol path for the VM.
    let base_dataset = format!("{}/{}", global_config.pool, global_config.dataset);
    let vm_zvol = format!("{base_dataset}/vms/{}", resolved.name);

    let mut rollback = Rollback::new();

    // Clone @base snapshot → per-VM zvol (instant, copy-on-write).
    let snapshot = format!("{image_zvol}@base");
    println!("Cloning {} → {}...", snapshot, vm_zvol);
    zfs::volume::clone(&snapshot, &vm_zvol)?;
    {
        let zvol = vm_zvol.clone();
        let sd = state_dir.to_path_buf();
        let name = resolved.name.clone();
        rollback.push("ZFS zvol clone", move || {
            let _ = zfs::volume::destroy(&zvol, true);
            let _ = vm::delete(&StateStore::new(sd), &name);
        });
    }

    create_post_clone(
        &resolved,
        &store,
        &mut global_config,
        &vm_zvol,
        image_size_mib,
        &image_ref,
    )?;

    rollback.commit();

    if !resolved.no_start {
        start(&StartArgs { name: resolved.name.clone() }, state_dir)?;
    }

    Ok(())
}

/// Post-clone steps: grow zvol, inject SSH key, save metadata.
///
/// Separated from [`create`] so the caller can clean up the zvol on failure.
fn create_post_clone(
    resolved: &ResolvedVmCreate,
    store: &StateStore,
    global_config: &mut GlobalConfig,
    vm_zvol: &str,
    image_size_mib: u64,
    image_ref: &str,
) -> anyhow::Result<()> {
    // Grow zvol if requested disk size exceeds image size.
    let requested_size_mib = resolved.disk_size as u64 * 1024;
    let needs_resize = requested_size_mib > image_size_mib;
    if needs_resize {
        println!("Growing zvol to {} GiB...", resolved.disk_size);
        zfs::volume::set_volsize(vm_zvol, resolved.disk_size)?;
    }

    // Wait for the zvol device node to appear.
    let dev_path = zfs::volume::device_path(vm_zvol);
    image::zvol::wait_for_device(&dev_path)?;

    // If we grew the zvol, expand the ext4 filesystem to fill the space.
    if needs_resize {
        println!("Expanding ext4 filesystem...");
        e2fsck(&dev_path)?;
        resize2fs(&dev_path)?;
    }

    // Mount the zvol to inject per-VM SSH key.
    let mount_dir = tempfile::tempdir().map_err(|e| Error::Io {
        path: std::env::temp_dir(),
        source: e,
    })?;

    mount_block_device(&dev_path, mount_dir.path())?;

    let inject_result = inject_ssh_key(mount_dir.path());
    let umount_result = umount(mount_dir.path());

    // Always try to unmount, even if injection failed.
    let detected_ssh_user = inject_result?;
    umount_result?;

    // Determine kernel path (auto-downloads default if needed).
    let kernel_path = ensure_kernel(&resolved.kernel, global_config, store)?;

    // Use YAML SSH overrides if provided, otherwise use auto-detected values.
    let ssh_user = resolved
        .ssh_user
        .clone()
        .unwrap_or(detected_ssh_user);
    let ssh_key = resolved.ssh_key.clone().unwrap_or_else(|| {
        image::inject::default_ssh_privkey_path()
            .unwrap_or_else(|| PathBuf::from("/root/.ssh/id_ed25519"))
    });

    // Build and save VM metadata.
    let metadata = VmMetadata {
        name: resolved.name.clone(),
        id: Uuid::new_v4(),
        status: VmStatus::Created,
        image: image_ref.to_string(),
        cpus: resolved.cpus,
        memory_mib: resolved.memory,
        disk_size_gib: resolved.disk_size,
        kernel_path,
        zvol_path: vm_zvol.to_string(),
        network: None,
        pid: None,
        api_socket: store.vm_dir(&resolved.name).join("firecracker.sock"),
        created_at: vm::now_iso8601(),
        ssh: SshConfig {
            user: ssh_user,
            key: ssh_key,
        },
    };

    vm::save(store, &metadata)?;

    println!("VM '{}' created successfully.", resolved.name);

    Ok(())
}

/// Start a VM: set up networking, spawn Firecracker, configure via API, boot.
///
/// Workflow: validate state → allocate IP → create TAP device → set iptables
/// → clean stale socket → spawn firecracker → wait for API socket
/// → configure machine (CPU, memory, kernel, rootfs, network) → start instance
/// → update metadata.
///
/// Uses a [`Rollback`] guard to ensure all resources (IP allocation, TAP device,
/// iptables rules, Firecracker process) are cleaned up if any step fails.
fn start(args: &StartArgs, state_dir: &Path) -> anyhow::Result<()> {
    use crate::cleanup::Rollback;

    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;

    // Load and validate VM state.
    let mut metadata = vm::load(&store, &args.name)?;
    match metadata.status {
        VmStatus::Created | VmStatus::Stopped => {}
        _ => {
            return Err(Error::VmWrongState {
                name: args.name.clone(),
                actual: metadata.status.to_string(),
                expected: "created or stopped".to_string(),
            }
            .into())
        }
    }

    let mut rollback = Rollback::new();

    // ── Networking ────────────────────────────────────────────────

    // Determine WAN interface (from config or auto-detect).
    let wan_iface = match &config.wan_iface {
        Some(iface) => iface.clone(),
        None => {
            println!("No WAN interface in config, detecting...");
            network::wan::detect()?
        }
    };

    // Allocate a /30 IP block for this VM.
    let subnet = network::ip::DEFAULT_SUBNET;
    println!("Allocating network address...");
    let allocation = network::ip::allocate(&store, subnet, &metadata.name)?;
    println!(
        "  Guest IP: {}, Host IP: {}",
        allocation.guest_ip, allocation.host_ip
    );
    {
        let sd = state_dir.to_path_buf();
        let name = metadata.name.clone();
        rollback.push("IP allocation", move || {
            let _ = network::ip::release(&StateStore::new(sd), &name);
        });
    }

    // Create TAP device.
    let tap_name = network::tap::device_name(&metadata.id);
    let host_ip_cidr = format!("{}/30", allocation.host_ip);
    println!("Creating TAP device {tap_name}...");
    network::tap::create(&tap_name, &host_ip_cidr)?;
    {
        let tap = tap_name.clone();
        rollback.push("TAP device", move || {
            let _ = network::tap::delete(&tap);
        });
    }

    // Enable IP forwarding (idempotent).
    network::nat::enable_ip_forwarding()?;

    // Add iptables NAT/forwarding rules.
    network::nat::add_rules(&tap_name, &allocation.guest_ip, &wan_iface)?;
    {
        let tap = tap_name.clone();
        let guest_ip = allocation.guest_ip.clone();
        let wan = wan_iface.clone();
        rollback.push("iptables rules", move || {
            let _ = network::nat::remove_rules(&tap, &guest_ip, &wan);
        });
    }

    let net_info = NetworkInfo {
        tap_device: tap_name,
        host_ip: allocation.host_ip,
        guest_ip: allocation.guest_ip,
        gateway_ip: allocation.gateway_ip,
        netmask: allocation.netmask,
        guest_mac: None,
        wan_iface: Some(wan_iface),
    };

    // ── Firecracker ───────────────────────────────────────────────

    let socket_path = &metadata.api_socket;
    let log_path = store.vm_dir(&args.name).join("firecracker.log");
    let rootfs_path = zfs::volume::device_path(&metadata.zvol_path);

    // Clean up stale socket from a previous run.
    if socket_path.exists() {
        std::fs::remove_file(socket_path).map_err(|e| Error::Io {
            path: socket_path.clone(),
            source: e,
        })?;
    }

    // Spawn Firecracker process.
    println!("Starting Firecracker...");
    let child = firecracker::process::spawn(socket_path, &log_path)?;
    let pid = child.id();
    rollback.push("Firecracker process", move || {
        let _ = firecracker::process::kill(pid);
    });

    // Configure and boot via the Firecracker API.
    start_configure(&metadata, socket_path, &rootfs_path, &net_info)?;

    // ── Persist state ─────────────────────────────────────────────

    metadata.network = Some(net_info);
    metadata.status = VmStatus::Running;
    metadata.pid = Some(pid);
    vm::save(&store, &metadata)?;

    // Everything succeeded — keep all resources.
    rollback.commit();

    println!("VM '{}' started (pid {}).", args.name, pid);
    Ok(())
}

/// Clean up networking resources on failure.
///
/// Best-effort: logs warnings but does not propagate errors, since we're
/// already handling a failure.
fn cleanup_network(store: &StateStore, vm_name: &str, net_info: &NetworkInfo) {
    // Use the stored WAN interface (matches what was used to create the rules),
    // falling back to re-detection for backwards compatibility with older metadata.
    let wan_iface = net_info
        .wan_iface
        .clone()
        .or_else(|| network::wan::detect().ok());
    if let Some(wan_iface) = wan_iface {
        let _ = network::nat::remove_rules(&net_info.tap_device, &net_info.guest_ip, &wan_iface);
    }
    let _ = network::tap::delete(&net_info.tap_device);
    let _ = network::ip::release(store, vm_name);
}

/// Configure and start a Firecracker instance via the API.
///
/// Runs the async API calls inside a one-shot tokio runtime.
fn start_configure(
    metadata: &VmMetadata,
    socket_path: &Path,
    rootfs_path: &Path,
    net_info: &NetworkInfo,
) -> anyhow::Result<()> {
    // Wait for the API socket to appear.
    firecracker::process::wait_for_socket(socket_path)?;

    // Detect host DNS servers for the guest, scoped to the WAN interface
    // so we only get servers reachable through the VM's NAT path.
    let wan_iface = net_info.wan_iface.as_deref().unwrap_or("eth0");
    let dns_servers = network::dns::detect_nameservers(wan_iface);
    println!(
        "Using DNS servers: {}",
        dns_servers.join(", ")
    );

    // Build VM configuration with networking.
    let vm_config = firecracker::config::VmConfig::new(
        metadata.cpus,
        metadata.memory_mib,
        &metadata.kernel_path,
        rootfs_path,
    )
    .with_network(firecracker::config::VmNetworkConfig {
        tap_device: net_info.tap_device.clone(),
        guest_ip: net_info.guest_ip.clone(),
        gateway_ip: net_info.gateway_ip.clone(),
        netmask: net_info.netmask.clone(),
        guest_mac: net_info.guest_mac.clone(),
        dns_servers,
    });

    // Run the async API calls.
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = firecracker::api::FirecrackerClient::new(socket_path);
        vm_config.configure_and_start(&client).await
    })
}

/// Stop a running VM: graceful shutdown via SendCtrlAltDel, then SIGKILL fallback.
///
/// Workflow: validate state → send CtrlAltDel (or skip if --force) → wait for exit
/// → SIGKILL if still alive → clean up network + socket → update metadata.
fn stop(args: &StopArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    // Load and validate VM state.
    let mut metadata = vm::load(&store, &args.name)?;
    match metadata.status {
        VmStatus::Running | VmStatus::Paused => {}
        _ => {
            return Err(Error::VmWrongState {
                name: args.name.clone(),
                actual: metadata.status.to_string(),
                expected: "running or paused".to_string(),
            }
            .into())
        }
    }

    let pid = metadata.pid.ok_or_else(|| {
        anyhow::anyhow!(
            "vm '{}' is {} but has no PID — state may be corrupted\n\
             Hint: try 'ember vm delete --force {}' and recreate the VM",
            args.name,
            metadata.status,
            args.name
        )
    })?;

    // Check if the process is actually alive.
    if !firecracker::process::is_alive(pid) {
        println!("Firecracker process (pid {pid}) is already dead.");
    } else if args.force {
        // --force: skip graceful shutdown, go straight to SIGKILL.
        println!("Force-killing Firecracker (pid {pid})...");
        firecracker::process::kill(pid)?;
        firecracker::process::wait_for_exit(pid, std::time::Duration::from_secs(5));
    } else {
        // Graceful shutdown: send CtrlAltDel via the API, then wait.
        println!("Sending shutdown signal to VM '{}'...", args.name);
        let socket_path = &metadata.api_socket;

        let send_result = if socket_path.exists() {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let client = firecracker::api::FirecrackerClient::new(socket_path);
                client
                    .put_action(&firecracker::api::InstanceAction::send_ctrl_alt_del())
                    .await
            })
        } else {
            Err(anyhow::anyhow!("API socket not found, falling back to SIGKILL"))
        };

        if let Err(e) = send_result {
            eprintln!("Graceful shutdown failed ({e}), sending SIGKILL...");
            firecracker::process::kill(pid)?;
            firecracker::process::wait_for_exit(pid, std::time::Duration::from_secs(5));
        } else {
            // Wait up to 10 seconds for the process to exit.
            println!("Waiting for VM to shut down (up to 10s)...");
            let exited =
                firecracker::process::wait_for_exit(pid, std::time::Duration::from_secs(10));
            if !exited {
                println!("VM did not exit in time, sending SIGKILL...");
                firecracker::process::kill(pid)?;
                firecracker::process::wait_for_exit(pid, std::time::Duration::from_secs(5));
            }
        }
    }

    // Clean up networking resources (TAP device, iptables rules, IP allocation).
    if let Some(ref net_info) = metadata.network {
        cleanup_network(&store, &args.name, net_info);
    }

    // Clean up the API socket.
    let socket_path = &metadata.api_socket;
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }

    // Update metadata.
    metadata.status = VmStatus::Stopped;
    metadata.pid = None;
    metadata.network = None;
    vm::save(&store, &metadata)?;

    println!("VM '{}' stopped.", args.name);
    Ok(())
}

/// Pause a running VM via the Firecracker PATCH /vm API.
///
/// Workflow: validate VM is running → send PATCH /vm { state: "Paused" } → update metadata.
/// Network and PID are preserved — the VM can be resumed or stopped from this state.
fn pause(args: &PauseArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    // Load and validate VM state — only running VMs can be paused.
    let mut metadata = vm::load(&store, &args.name)?;
    match metadata.status {
        VmStatus::Running => {}
        _ => {
            return Err(Error::VmWrongState {
                name: args.name.clone(),
                actual: metadata.status.to_string(),
                expected: "running".to_string(),
            }
            .into())
        }
    }

    let socket_path = &metadata.api_socket;
    if !socket_path.exists() {
        anyhow::bail!(
            "vm '{}' is marked as running but API socket not found at {}\n\
             Hint: the Firecracker process may have crashed — try 'ember vm stop --force {}' and restart",
            args.name,
            socket_path.display(),
            args.name
        );
    }

    println!("Pausing VM '{}'...", args.name);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = firecracker::api::FirecrackerClient::new(socket_path);
        client
            .patch_vm(&firecracker::api::VmStateUpdate::pause())
            .await
    })?;

    metadata.status = VmStatus::Paused;
    vm::save(&store, &metadata)?;

    println!("VM '{}' paused.", args.name);
    Ok(())
}

/// Resume a paused VM via the Firecracker PATCH /vm API.
///
/// Workflow: validate VM is paused → send PATCH /vm { state: "Resumed" } → update metadata.
/// Network and PID were preserved during pause, so the VM resumes exactly where it left off.
fn resume(args: &ResumeArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    // Load and validate VM state — only paused VMs can be resumed.
    let mut metadata = vm::load(&store, &args.name)?;
    match metadata.status {
        VmStatus::Paused => {}
        _ => {
            return Err(Error::VmWrongState {
                name: args.name.clone(),
                actual: metadata.status.to_string(),
                expected: "paused".to_string(),
            }
            .into())
        }
    }

    let socket_path = &metadata.api_socket;
    if !socket_path.exists() {
        anyhow::bail!(
            "vm '{}' is marked as paused but API socket not found at {}\n\
             Hint: the Firecracker process may have crashed — try 'ember vm stop --force {}' and restart",
            args.name,
            socket_path.display(),
            args.name
        );
    }

    println!("Resuming VM '{}'...", args.name);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = firecracker::api::FirecrackerClient::new(socket_path);
        client
            .patch_vm(&firecracker::api::VmStateUpdate::resume())
            .await
    })?;

    metadata.status = VmStatus::Running;
    vm::save(&store, &metadata)?;

    println!("VM '{}' resumed.", args.name);
    Ok(())
}

/// Grow a stopped VM's disk.
///
/// Workflow: enforce stopped/created state → check new size > current
/// → grow zvol → expand ext4 → update metadata.
fn resize(args: &ResizeArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let mut metadata = vm::load(&store, &args.name)?;

    // Enforce VM is not running or paused.
    match metadata.status {
        VmStatus::Created | VmStatus::Stopped => {}
        VmStatus::Running => {
            anyhow::bail!(
                "vm '{}' is running — stop it before resizing",
                args.name
            );
        }
        VmStatus::Paused => {
            anyhow::bail!(
                "vm '{}' is paused — stop it before resizing",
                args.name
            );
        }
    }

    // Convert size with unit to GiB.
    let new_gib = args
        .disk_size
        .to_gib()
        .map_err(|e| anyhow::anyhow!("invalid disk size: {e}"))?;

    // Enforce grow-only (shrinking is not supported).
    let current_gib = metadata.disk_size_gib;
    if new_gib <= current_gib {
        anyhow::bail!(
            "new disk size ({} GiB) must be larger than current size ({} GiB)",
            new_gib,
            current_gib
        );
    }

    // Grow the ZFS zvol.
    println!("Growing zvol to {} GiB...", new_gib);
    zfs::volume::set_volsize(&metadata.zvol_path, new_gib)?;

    // Wait for the block device node to settle after the resize.
    let dev_path = zfs::volume::device_path(&metadata.zvol_path);
    image::zvol::wait_for_device(&dev_path)?;

    // Expand the ext4 filesystem to fill the new space.
    println!("Expanding ext4 filesystem...");
    e2fsck(&dev_path)?;
    resize2fs(&dev_path)?;

    // Update metadata.
    metadata.disk_size_gib = new_gib;
    vm::save(&store, &metadata)?;

    println!(
        "VM '{}' disk resized from {} GiB to {} GiB.",
        args.name, current_gib, new_gib
    );
    Ok(())
}

/// Delete a VM and all its resources.
///
/// Workflow: force-stop if running (requires --force) → clean up network →
/// destroy ZFS zvol (recursively, including user snapshots) → remove state
/// directory.
///
/// Each cleanup step is idempotent — continues if the resource is already gone.
fn delete(args: &DeleteArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    // Load VM metadata (must exist).
    let metadata = vm::load(&store, &args.name)?;

    // If the VM is running or paused, require --force.
    match metadata.status {
        VmStatus::Running | VmStatus::Paused => {
            if !args.force {
                anyhow::bail!(
                    "vm '{}' is {} — stop it first or use --force",
                    args.name,
                    metadata.status
                );
            }

            // Force-kill the Firecracker process and wait for it to die
            // so it releases the zvol block device before we destroy it.
            if let Some(pid) = metadata.pid {
                if firecracker::process::is_alive(pid) {
                    println!("Force-killing Firecracker (pid {pid})...");
                    firecracker::process::kill(pid)?;
                    firecracker::process::wait_for_exit(pid, std::time::Duration::from_secs(5));
                }
            }

            // Clean up the API socket.
            if metadata.api_socket.exists() {
                let _ = std::fs::remove_file(&metadata.api_socket);
            }
        }
        VmStatus::Created | VmStatus::Stopped => {}
    }

    // Clean up networking resources (TAP device, iptables rules, IP allocation).
    // Idempotent — safe to call even if resources are already gone.
    if let Some(ref net_info) = metadata.network {
        cleanup_network(&store, &metadata.name, net_info);
    }

    // Wait for udev to finish processing device events. After mount/unmount
    // cycles (e.g. SSH key injection during create), the zvol block device may
    // still be briefly held by the kernel. Without this, `zfs destroy` can
    // fail with "device busy".
    let _ = ProcessCommand::new("udevadm")
        .arg("settle")
        .status();

    // Destroy the ZFS zvol and all snapshots under it.
    println!("Destroying ZFS zvol '{}'...", metadata.zvol_path);
    match zfs::volume::destroy(&metadata.zvol_path, true) {
        Ok(()) => {}
        Err(e) => {
            // Log but continue — the zvol may already be gone.
            eprintln!("Warning: failed to destroy zvol '{}': {e}", metadata.zvol_path);
        }
    }

    // Remove the VM state directory.
    vm::delete(&store, &args.name)?;

    println!("VM '{}' deleted.", args.name);
    Ok(())
}

/// List all VMs with summary information.
fn list(args: &ListArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let vms = vm::list(&store)?;

    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&vms)?);
        }
        OutputFormat::Table => {
            if vms.is_empty() {
                println!("No VMs found. Create one with: ember vm create <name> --image <image>");
                return Ok(());
            }

            println!(
                "{:<20} {:<10} {:<40} {:>4} {:>6} {:>5}",
                "NAME", "STATUS", "IMAGE", "CPUS", "MEM", "DISK"
            );
            for vm in &vms {
                println!(
                    "{:<20} {:<10} {:<40} {:>4} {:>4}Mi {:>3}Gi",
                    vm.name,
                    vm.status,
                    vm.image,
                    vm.cpus,
                    vm.memory_mib,
                    vm.disk_size_gib,
                );
            }
        }
    }

    Ok(())
}

/// Show detailed information about a single VM.
fn inspect(args: &InspectArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let metadata = vm::load(&store, &args.name)?;

    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&metadata)?);
        }
        OutputFormat::Table => {
            println!("Name:        {}", metadata.name);
            println!("ID:          {}", metadata.id);
            println!("Status:      {}", metadata.status);
            println!("Image:       {}", metadata.image);
            println!("CPUs:        {}", metadata.cpus);
            println!("Memory:      {} MiB", metadata.memory_mib);
            println!("Disk:        {} GiB", metadata.disk_size_gib);
            println!("Kernel:      {}", metadata.kernel_path.display());
            println!("ZFS zvol:    {}", metadata.zvol_path);
            println!("API socket:  {}", metadata.api_socket.display());
            println!("Created:     {}", metadata.created_at);

            if let Some(pid) = metadata.pid {
                println!("PID:         {}", pid);
            }

            if let Some(ref net) = metadata.network {
                println!("Network:");
                println!("  TAP device:  {}", net.tap_device);
                println!("  Host IP:     {}", net.host_ip);
                println!("  Guest IP:    {}", net.guest_ip);
                println!("  Netmask:     {}", net.netmask);
                if let Some(ref mac) = net.guest_mac {
                    println!("  Guest MAC:   {}", mac);
                }
            }

            println!("SSH:");
            println!("  User:        {}", metadata.ssh.user);
            println!("  Key:         {}", metadata.ssh.key.display());
        }
    }

    Ok(())
}

/// Open an SSH session to a running VM.
///
/// Invokes the system `ssh` command for full interactive terminal support
/// (PTY, resize, escape sequences). If a command is given after `--`, it
/// is passed to ssh for non-interactive execution.
fn ssh(args: &SshArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let metadata = vm::load(&store, &args.name)?;

    if metadata.status != VmStatus::Running {
        anyhow::bail!(
            "vm '{}' is {} — start it first with: ember vm start {}",
            args.name,
            metadata.status,
            args.name
        );
    }

    let network = metadata.network.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "vm '{}' has no network configured — cannot connect via SSH",
            args.name
        )
    })?;

    let guest_ip = &network.guest_ip;
    let user = &metadata.ssh.user;
    let key_path = &metadata.ssh.key;

    let mut cmd = ProcessCommand::new("ssh");
    cmd.args([
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR",
        "-i", &key_path.to_string_lossy(),
        &format!("{user}@{guest_ip}"),
    ]);

    if !args.command.is_empty() {
        cmd.args(&args.command);
    }

    let status = cmd.status().map_err(|e| {
        anyhow::anyhow!(
            "failed to execute ssh: {e}\n\
             Hint: is the 'ssh' client installed and in PATH?"
        )
    })?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

/// Inject the invoking user's SSH public key into the rootfs.
///
/// Detects whether the rootfs has an `ubuntu` user and injects the key
/// into the appropriate home directory. Returns the detected SSH user name.
fn inject_ssh_key(rootfs_dir: &Path) -> anyhow::Result<String> {
    let pubkey_path = image::inject::default_ssh_pubkey_path().ok_or_else(|| {
        anyhow::anyhow!(
            "no SSH public key found at ~/.ssh/id_ed25519.pub or ~/.ssh/id_rsa.pub\n\
             Hint: create one with: ssh-keygen -t ed25519"
        )
    })?;

    let (user, home_relative) = image::inject::detect_ssh_user(rootfs_dir);

    println!("Injecting SSH key from {} (user: {user})...", pubkey_path.display());
    image::inject::inject_ssh_authorized_keys_for_home(rootfs_dir, &pubkey_path, home_relative)?;

    Ok(user.to_string())
}

/// Mount a block device at the given mount point.
fn mount_block_device(device: &Path, mount_dir: &Path) -> crate::error::Result<()> {
    let output = ProcessCommand::new("mount")
        .arg(device)
        .arg(mount_dir)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "mount".to_string(),
            source: e,
        })?;

    Error::check_command("mount", output)?;
    Ok(())
}

/// Unmount a filesystem.
fn umount(mount_dir: &Path) -> crate::error::Result<()> {
    let output = ProcessCommand::new("umount")
        .arg(mount_dir)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "umount".to_string(),
            source: e,
        })?;

    Error::check_command("umount", output)?;
    Ok(())
}

/// Check ext4 filesystem consistency before resize.
///
/// `-f` forces checking even if the filesystem is marked clean.
/// `-p` automatically repairs safe issues without prompting.
fn e2fsck(device: &Path) -> crate::error::Result<()> {
    let output = ProcessCommand::new("e2fsck")
        .args(["-f", "-p"])
        .arg(device)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "e2fsck".to_string(),
            source: e,
        })?;

    // e2fsck exit codes: 0 = clean, 1 = errors corrected. Both are OK.
    let code = output.status.code().unwrap_or(-1);
    if code > 1 {
        return Err(Error::Command {
            command: "e2fsck".to_string(),
            exit_code: code,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    Ok(())
}

/// Expand an ext4 filesystem to fill its block device.
fn resize2fs(device: &Path) -> crate::error::Result<()> {
    let output = ProcessCommand::new("resize2fs")
        .arg(device)
        .output()
        .map_err(|e| Error::CommandExec {
            command: "resize2fs".to_string(),
            source: e,
        })?;

    Error::check_command("resize2fs", output)?;
    Ok(())
}
