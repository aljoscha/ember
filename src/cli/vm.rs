use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use uuid::Uuid;

use super::init::GlobalConfig;
use crate::backend::{Network, NetworkBackend, Storage, StorageBackend, Vm, VmBackend};
use crate::config;
use crate::config::size::ByteSize;
use crate::error::Error;
use crate::image;
use crate::image::registry::ImageRegistry;
use crate::state::store::StateStore;
use crate::state::vm::{self, NetworkInfo, SshConfig, VmMetadata, VmStatus};

/// Load a running VM with network info, checking that the guest IP is resolved.
///
/// Wraps `vm::load_running_with_network` and returns an error if the guest IP
/// is still "pending" (macOS vmnet DHCP hasn't been discovered yet).
/// State reconciliation normally resolves pending IPs before this is called.
pub fn load_running_with_ip(
    store: &StateStore,
    name: &str,
) -> anyhow::Result<(VmMetadata, NetworkInfo)> {
    let (metadata, network) = vm::load_running_with_network(store, name)?;

    if network.guest_ip == "pending" {
        anyhow::bail!(
            "guest IP not yet available for '{name}' — the VM may still be booting\n\
             Hint: try 'ember reconcile' or wait a few seconds and retry"
        );
    }

    Ok((metadata, network))
}

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

    /// Fork an existing VM into a new independent VM
    Fork(ForkArgs),
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
pub struct ForkArgs {
    /// Source VM to fork from
    pub source: String,

    /// New VM name
    pub name: String,

    /// Number of vCPUs
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory size, e.g. 512M, 16G
    #[arg(long)]
    pub memory: Option<ByteSize>,

    /// Disk size, e.g. 8G (must be >= source)
    #[arg(long)]
    pub disk_size: Option<ByteSize>,

    /// Kernel preset or file path [presets: stock]
    #[arg(long)]
    pub kernel: Option<crate::kernel::KernelSpec>,

    /// Network subnet
    #[arg(long)]
    pub network: Option<String>,

    /// Don't start the VM after forking
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Args)]
pub struct InspectArgs {
    /// VM name
    pub name: String,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
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
        VmCommand::Fork(args) => fork(args, state_dir),
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
    /// Custom boot arguments from YAML config.
    boot_args: Option<String>,
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

    let kernel = args
        .kernel
        .clone()
        .or_else(|| yaml.and_then(|c| c.kernel.clone()));

    let boot_args = yaml.and_then(|c| c.boot_args.clone());

    let network = args
        .network
        .clone()
        .or_else(|| yaml.and_then(|c| c.network.as_ref().and_then(|n| n.subnet.clone())));

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
        boot_args,
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

    let image_name = image_entry.local_name.clone();
    let image_ref = image_entry.reference.clone();
    let image_size_mib = image_entry.size_mib;

    let storage = Storage::new(&global_config);

    let mut rollback = Rollback::new();

    // Clone base image → per-VM disk (instant, copy-on-write).
    println!("Cloning image for VM '{}'...", resolved.name);
    let vm_disk_path = storage.clone_for_vm(&image_name, &resolved.name)?;
    let vm_zvol = vm_disk_path.to_string_lossy().to_string();
    {
        let storage = storage.clone();
        let sd = state_dir.to_path_buf();
        let name = resolved.name.clone();
        rollback.push("VM storage clone", move || {
            let _ = storage.destroy_vm_storage(&name);
            let _ = vm::delete(&StateStore::new(sd), &name);
        });
    }

    create_post_clone(
        &resolved,
        &store,
        &mut global_config,
        &storage,
        &vm_zvol,
        image_size_mib,
        &image_ref,
    )?;

    rollback.commit();

    if !resolved.no_start {
        start(
            &StartArgs {
                name: resolved.name.clone(),
            },
            state_dir,
        )?;
    }

    Ok(())
}

/// Post-clone steps: grow disk, inject SSH key, save metadata.
///
/// Separated from [`create`] so the caller can clean up storage on failure.
fn create_post_clone(
    resolved: &ResolvedVmCreate,
    store: &StateStore,
    global_config: &mut GlobalConfig,
    storage: &Storage,
    vm_zvol: &str,
    image_size_mib: u64,
    image_ref: &str,
) -> anyhow::Result<()> {
    // Grow disk if requested disk size exceeds image size.
    let requested_size_mib = resolved.disk_size as u64 * 1024;
    let needs_resize = requested_size_mib > image_size_mib;
    if needs_resize {
        println!("Growing disk to {} GiB...", resolved.disk_size);
        storage.resize(
            &resolved.name,
            ByteSize::from_gib(resolved.disk_size as u64),
        )?;
    }

    // Inject per-VM SSH key into the rootfs image.
    // Linux: mounts the zvol, writes the key, unmounts.
    // macOS: uses debugfs to write directly into the ext4 image.
    let dev_path = storage.disk_device_path(&resolved.name);
    let pubkey_path = image::inject::default_ssh_pubkey_path().ok_or_else(|| {
        anyhow::anyhow!(
            "no SSH public key found at ~/.ssh/id_ed25519.pub or ~/.ssh/id_rsa.pub\n\
             Hint: create one with: ssh-keygen -t ed25519"
        )
    })?;
    println!("Injecting SSH key from {}...", pubkey_path.display());
    let detected_ssh_user = storage.inject_ssh_key(&dev_path, &pubkey_path)?;

    // Determine kernel path (auto-downloads default if needed).
    let kernel_path = ensure_kernel(&resolved.kernel, global_config, store)?;

    // Use YAML SSH overrides if provided, otherwise use auto-detected values.
    let ssh_user = resolved.ssh_user.clone().unwrap_or(detected_ssh_user);
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
        boot_args: resolved.boot_args.clone(),
        subnet: resolved.network.clone(),
        network: None,
        pid: None,
        api_socket: store.vm_dir(&resolved.name).join("firecracker.sock"),
        created_at: vm::now_iso8601(),
        ssh: SshConfig {
            user: ssh_user,
            key: ssh_key,
        },
        forked_from: None,
    };

    vm::save(store, &metadata)?;

    println!("VM '{}' created successfully.", resolved.name);

    Ok(())
}

/// Fork an existing VM into a new independent VM.
///
/// Workflow: validate source is stopped → snapshot source zvol → COW clone into
/// new VM → optionally grow disk → resolve kernel → save metadata → optionally start.
///
/// No SSH key injection — the forked disk already has keys from the source VM.
fn fork(args: &ForkArgs, state_dir: &Path) -> anyhow::Result<()> {
    use crate::cleanup::Rollback;

    let store = StateStore::new(state_dir.to_path_buf());
    let mut global_config: GlobalConfig = store.read(&store.config_path())?;

    // Source must exist and be stopped.
    let source = vm::require_stopped(&store, &args.source, "forking")?;

    // Target must not exist.
    if vm::exists(&store, &args.name) {
        anyhow::bail!("vm '{}' already exists", args.name);
    }

    // Resolve config: source as defaults, CLI flags override.
    let cpus = args.cpus.unwrap_or(source.cpus);

    let memory_mib = match args.memory {
        Some(m) => m
            .to_mib()
            .map_err(|e| anyhow::anyhow!("invalid memory size: {e}"))?,
        None => source.memory_mib,
    };

    let disk_size_gib = match args.disk_size {
        Some(d) => {
            let gib = d
                .to_gib()
                .map_err(|e| anyhow::anyhow!("invalid disk size: {e}"))?;
            if gib < source.disk_size_gib {
                anyhow::bail!(
                    "cannot shrink disk: requested {} GiB but source is {} GiB",
                    gib,
                    source.disk_size_gib
                );
            }
            gib
        }
        None => source.disk_size_gib,
    };

    let subnet = args.network.clone().or(source.subnet.clone());

    let storage = Storage::new(&global_config);

    // Snapshot source and clone into new VM via the storage backend.
    let fork_snap_name = format!("fork-{}", args.name);
    println!("Forking '{}' → '{}'...", args.source, args.name);
    let (vm_disk_path, fork_snap_full) =
        storage.clone_from_snapshot(&args.source, &fork_snap_name, &args.name)?;
    let vm_zvol = vm_disk_path.to_string_lossy().to_string();

    let mut rollback = Rollback::new();
    {
        let storage = storage.clone();
        let fork_origin = fork_snap_full.clone();
        let sd = state_dir.to_path_buf();
        let name = args.name.clone();
        rollback.push("fork clone + snapshot", move || {
            let _ = storage.destroy_vm_storage(&name);
            let _ = storage.destroy_fork_origin(&fork_origin);
            let _ = vm::delete(&StateStore::new(sd), &name);
        });
    }

    // Grow disk if requested.
    let needs_resize = disk_size_gib > source.disk_size_gib;
    if needs_resize {
        println!("Growing disk to {} GiB...", disk_size_gib);
        storage.resize(&args.name, ByteSize::from_gib(disk_size_gib as u64))?;
    }

    // Resolve kernel: CLI override or inherit from source.
    let kernel_path = if args.kernel.is_some() {
        ensure_kernel(&args.kernel, &mut global_config, &store)?
    } else {
        source.kernel_path.clone()
    };

    // Build metadata inheriting from source.
    let metadata = VmMetadata {
        name: args.name.clone(),
        id: Uuid::new_v4(),
        status: VmStatus::Created,
        image: source.image.clone(),
        cpus,
        memory_mib,
        disk_size_gib,
        kernel_path,
        zvol_path: vm_zvol,
        boot_args: source.boot_args.clone(),
        subnet,
        network: None,
        pid: None,
        api_socket: store.vm_dir(&args.name).join("firecracker.sock"),
        created_at: vm::now_iso8601(),
        ssh: source.ssh.clone(),
        forked_from: Some(fork_snap_full),
    };

    vm::save(&store, &metadata)?;

    rollback.commit();

    println!("VM '{}' forked from '{}'.", args.name, args.source);

    if !args.no_start {
        start(
            &StartArgs {
                name: args.name.clone(),
            },
            state_dir,
        )?;
    }

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

    let net_backend = Network::new(store.clone());
    println!("Setting up network...");
    let net_info = net_backend.setup(&metadata, &config)?;
    println!(
        "  Guest IP: {}, Host IP: {}",
        net_info.guest_ip, net_info.host_ip
    );
    {
        let net = Network::new(StateStore::new(state_dir.to_path_buf()));
        let meta_name = metadata.name.clone();
        let net_info_clone = net_info.clone();
        rollback.push("network", move || {
            let teardown_meta = VmMetadata {
                name: meta_name,
                network: Some(net_info_clone),
                ..VmMetadata::default_for_teardown()
            };
            let _ = net.teardown(&teardown_meta);
        });
    }

    metadata.network = Some(net_info);

    // ── Hypervisor ────────────────────────────────────────────────

    println!("Starting VM...");
    let started = Vm::start(&metadata, &config)?;
    let pid = started.pid;
    {
        let meta = metadata.clone();
        rollback.push("VM process", move || {
            let _ = Vm::force_stop(&meta);
        });
    }

    // Merge the network info from the VM backend (contains the MAC address
    // assigned by the hypervisor) back into the metadata.
    metadata.network = Some(started.network.clone());

    // ── Guest IP discovery (macOS) ────────────────────────────────
    // On macOS, the guest IP is assigned by vmnet DHCP after boot.
    // Retry a few times since the lease may not appear immediately.
    #[cfg(target_os = "macos")]
    if let Some(ref mac) = started.network.guest_mac {
        print!("Waiting for guest IP...");
        let mut discovered_ip = None;
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            match net_backend.discover_guest_ip(mac) {
                Ok(ip) => {
                    discovered_ip = Some(ip);
                    break;
                }
                Err(_) => continue,
            }
        }
        if let Some(ip) = discovered_ip {
            println!(" {ip}");
            if let Some(ref mut net) = metadata.network {
                net.guest_ip = ip;
            }
        } else {
            println!(" not found (try 'ember vm inspect' later)");
        }
    }

    // ── Persist state ─────────────────────────────────────────────

    metadata.status = VmStatus::Running;
    metadata.pid = Some(pid);
    vm::save(&store, &metadata)?;

    // Everything succeeded — keep all resources.
    rollback.commit();

    println!("VM '{}' started (pid {}).", args.name, pid);
    Ok(())
}

/// Stop a running VM: graceful shutdown via SSH poweroff, then SIGKILL fallback.
///
/// Workflow: validate state → SSH poweroff (or SendCtrlAltDel fallback, or skip
/// if --force) → wait for exit → SIGKILL if still alive → clean up network +
/// socket → update metadata.
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

    // Stop the VM via the backend.
    if !Vm::is_running(pid) {
        println!("VM process (pid {pid}) is already dead.");
    } else if args.force {
        println!("Force-stopping VM (pid {pid})...");
        Vm::force_stop(&metadata)?;
    } else {
        println!("Stopping VM '{}'...", args.name);
        Vm::stop(&metadata)?;
    }

    // Clean up networking via the backend.
    let net_backend = Network::new(store.clone());
    let _ = net_backend.teardown(&metadata);

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

    // On Linux, verify the Firecracker API socket exists before attempting pause.
    // On macOS, pause uses signals to the ember-vz process (no socket needed).
    #[cfg(target_os = "linux")]
    {
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
    }

    println!("Pausing VM '{}'...", args.name);
    Vm::pause(&metadata)?;

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

    // On Linux, verify the Firecracker API socket exists before attempting resume.
    // On macOS, resume uses signals to the ember-vz process (no socket needed).
    #[cfg(target_os = "linux")]
    {
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
    }

    println!("Resuming VM '{}'...", args.name);
    Vm::resume(&metadata)?;

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
    let mut metadata = vm::require_stopped(&store, &args.name, "resizing")?;

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

    // Grow the disk via the storage backend (handles resize + ext4 expand).
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);
    println!("Resizing disk to {} GiB...", new_gib);
    storage.resize(&args.name, args.disk_size)?;

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
    if matches!(metadata.status, VmStatus::Running | VmStatus::Paused) && !args.force {
        anyhow::bail!(
            "vm '{}' is {} — stop it first or use --force",
            args.name,
            metadata.status
        );
    }

    force_delete_vm(&store, &metadata)?;
    Ok(())
}

/// Force-delete a VM: kill process, clean up network, destroy storage, remove state.
///
/// Idempotent — each cleanup step continues if the resource is already gone.
/// Called from `vm delete --force` and `image delete --force`.
pub fn force_delete_vm(store: &StateStore, metadata: &VmMetadata) -> anyhow::Result<()> {
    // Kill the hypervisor process if the VM is running/paused.
    if matches!(metadata.status, VmStatus::Running | VmStatus::Paused) {
        if let Some(pid) = metadata.pid {
            if Vm::is_running(pid) {
                println!("Force-stopping VM (pid {pid})...");
                let _ = Vm::force_stop(metadata);
            }
        }
        if metadata.api_socket.exists() {
            let _ = std::fs::remove_file(&metadata.api_socket);
        }
    }

    // Clean up networking via the backend.
    let net_backend = Network::new(store.clone());
    let _ = net_backend.teardown(metadata);

    // Wait for udev to finish processing device events.
    let _ = std::process::Command::new("udevadm").arg("settle").status();

    // Destroy storage via the backend.
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);

    println!("Destroying storage for VM '{}'...", metadata.name);
    let _ = storage.destroy_vm_storage(&metadata.name);

    // Clean up fork origin snapshot if this VM was forked.
    if let Some(ref fork_snap) = metadata.forked_from {
        let _ = storage.destroy_fork_origin(fork_snap);
    }

    // Remove the VM state directory.
    vm::delete(store, &metadata.name)?;

    println!("VM '{}' deleted.", metadata.name);
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
                    vm.name, vm.status, vm.image, vm.cpus, vm.memory_mib, vm.disk_size_gib,
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
            #[cfg(target_os = "linux")]
            {
                println!("ZFS zvol:    {}", metadata.zvol_path);
                println!("API socket:  {}", metadata.api_socket.display());
            }
            #[cfg(target_os = "macos")]
            {
                println!("Disk image:  {}", metadata.zvol_path);
            }
            println!("Created:     {}", metadata.created_at);

            if let Some(pid) = metadata.pid {
                println!("PID:         {}", pid);
            }

            if let Some(ref net) = metadata.network {
                println!("Network:");
                #[cfg(target_os = "linux")]
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
