use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use uuid::Uuid;

use super::fmt::{format_bytes_binary, GIB, MIB};
use crate::backend::{
    CurrentPlatform, Network, NetworkBackend, Platform, Storage, StorageBackend, Vm, VmBackend,
};
use crate::image;
use ember_core::config;
use ember_core::config::size::ByteSize;
use ember_core::config::GlobalConfig;
use ember_core::error::Error;
use ember_core::image::registry::ImageRegistry;
use ember_core::state::store::StateStore;
use ember_core::state::vm::{self, NetworkInfo, SshConfig, VmMetadata, VmStatus};

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

    /// Update a stopped VM's configuration
    UpdateConfig(UpdateConfigArgs),

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
    pub kernel: Option<ember_core::kernel::KernelSpec>,

    /// Network subnet
    #[arg(long)]
    pub network: Option<String>,

    /// VM config YAML file
    #[arg(long = "vm-config")]
    pub vm_config: Option<PathBuf>,

    /// Enable vsock device for host-guest communication
    #[arg(long)]
    pub vsock: bool,

    /// Don't start the VM after creation
    #[arg(long)]
    pub no_start: bool,

    /// Wait for VM to be SSH-reachable after start (seconds, 0 to skip)
    #[arg(long, default_value = "90")]
    pub wait: u64,

    /// Output format (json prints VM metadata on success)
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct StartArgs {
    /// VM name
    pub name: String,
}

#[derive(Args)]
pub struct StopArgs {
    /// VM name (required unless --all is used)
    #[arg(required_unless_present = "all")]
    pub name: Option<String>,

    /// Stop all running VMs
    #[arg(long, conflicts_with = "name")]
    pub all: bool,

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
pub struct UpdateConfigArgs {
    /// VM name
    pub name: String,

    /// Number of vCPUs
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory size, e.g. 512M, 16G
    #[arg(long)]
    pub memory: Option<ByteSize>,

    /// Kernel preset or file path [presets: stock]
    #[arg(long)]
    pub kernel: Option<ember_core::kernel::KernelSpec>,

    /// Kernel boot arguments (replaces current; use "" to clear)
    #[arg(long)]
    pub boot_args: Option<String>,

    /// SSH user
    #[arg(long)]
    pub ssh_user: Option<String>,

    /// SSH private key path
    #[arg(long)]
    pub ssh_key: Option<PathBuf>,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// VM name (required unless --all is used)
    #[arg(required_unless_present = "all")]
    pub name: Option<String>,

    /// Delete all VMs
    #[arg(long, conflicts_with = "name")]
    pub all: bool,

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
    pub kernel: Option<ember_core::kernel::KernelSpec>,

    /// Network subnet
    #[arg(long)]
    pub network: Option<String>,

    /// Enable vsock device for host-guest communication
    #[arg(long)]
    pub vsock: bool,

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
        VmCommand::UpdateConfig(args) => update_config(args, state_dir),
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
struct ResolvedVmCreate {
    name: String,
    image: String,
    cpus: u32,
    memory: u32,
    disk_size: u32,
    kernel: Option<ember_core::kernel::KernelSpec>,
    /// Custom boot arguments from YAML config.
    boot_args: Option<String>,
    /// Network subnet from YAML config (used during `start`, not `create`).
    network: Option<String>,
    no_start: bool,
    /// Seconds to wait for SSH after start (0 = don't wait).
    wait: u64,
    /// Output format.
    format: OutputFormat,
    /// SSH user override from YAML config.
    ssh_user: Option<String>,
    /// SSH private key override from YAML config.
    ssh_key: Option<PathBuf>,
    /// Whether vsock is enabled for this VM.
    vsock: bool,
}

/// Maximum Unix domain socket path length.
/// macOS: 104 bytes, Linux: 108 bytes. Use the smaller to be safe.
const MAX_UDS_PATH_LEN: usize = 104;

impl ResolvedVmCreate {
    /// Build a `VsockInfo` if vsock is enabled, allocating a unique CID.
    fn vsock_info(&self, store: &StateStore) -> anyhow::Result<Option<vm::VsockInfo>> {
        if self.vsock {
            let uds_path = store.vm_dir(&self.name).join("vsock.sock");
            validate_uds_path(&uds_path)?;
            let cid = ember_core::state::vsock::allocate(store, &self.name)?;
            Ok(Some(vm::VsockInfo {
                uds_path,
                guest_cid: cid,
            }))
        } else {
            Ok(None)
        }
    }
}

/// Validate that a UDS path doesn't exceed the OS limit for `sockaddr_un.sun_path`.
fn validate_uds_path(path: &Path) -> anyhow::Result<()> {
    let path_str = path.to_string_lossy();
    if path_str.len() >= MAX_UDS_PATH_LEN {
        anyhow::bail!(
            "vsock UDS path is too long ({} bytes, max {}):\n  {}\n\
             Hint: use a shorter --state-dir or VM name",
            path_str.len(),
            MAX_UDS_PATH_LEN - 1,
            path_str,
        );
    }
    Ok(())
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

    let vsock = args.vsock || yaml.and_then(|c| c.vsock).unwrap_or(false);

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
        wait: args.wait,
        format: args.format.clone(),
        ssh_user,
        ssh_key,
        vsock,
    })
}

/// Resolve the kernel path: CLI/YAML spec → global config → auto-download default preset.
fn ensure_kernel(
    cli_kernel: &Option<ember_core::kernel::KernelSpec>,
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
    let default_spec = ember_core::kernel::KernelSpec::Preset(ember_core::kernel::DEFAULT_PRESET);
    let dest = default_spec.resolve(store)?;

    // Persist so future creates skip the download.
    config.kernel_path = Some(dest.clone());
    store.write(&store.config_path(), config)?;

    Ok(dest)
}

/// Create a new VM from an image.
///
/// Workflow: load YAML config (if provided) → merge with CLI flags →
/// look up image → clone base image → grow disk if needed
/// → inject per-VM SSH key → save metadata.
///
/// Uses a [`Rollback`] guard to ensure the disk clone and state directory
/// are cleaned up if any step after cloning fails.
fn create(args: &CreateArgs, state_dir: &Path) -> anyhow::Result<()> {
    use ember_core::cleanup::Rollback;

    let store = StateStore::new(state_dir.to_path_buf());
    let mut global_config: GlobalConfig = store.read(&store.config_path())?;

    // Load YAML config if provided.
    let yaml_config = match &args.vm_config {
        Some(path) => {
            eprintln!("Loading VM config from {}...", path.display());
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
                "image '{}' not found locally\n\
                 \n  Build from Dockerfile:  ember image build {} -f <Dockerfile>\
                 \n  Pull from registry:     ember image pull {}\
                 \n  List local images:      ember image list",
                resolved.image,
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
    eprintln!("Cloning image for VM '{}'...", resolved.name);
    let vm_disk_path = storage.clone_for_vm(&image_name, &resolved.name)?;
    let vm_disk = vm_disk_path.to_string_lossy().to_string();
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
        &vm_disk,
        image_size_mib,
        &image_ref,
    )?;

    rollback.commit();

    if !resolved.no_start {
        if let Err(e) = start(
            &StartArgs {
                name: resolved.name.clone(),
            },
            state_dir,
        ) {
            // Start failed — clean up the created VM so we don't leave
            // orphaned state behind.
            eprintln!("Start failed, cleaning up VM '{}'...", resolved.name);
            let _ = delete(
                &DeleteArgs {
                    name: Some(resolved.name.clone()),
                    all: false,
                    force: true,
                },
                state_dir,
            );
            return Err(e);
        }

        // Wait for SSH to become reachable (if --wait > 0).
        if resolved.wait > 0 {
            wait_for_ssh(&store, &resolved.name, resolved.wait)?;
        }
    }

    // Output result
    match resolved.format {
        OutputFormat::Json => {
            let metadata = vm::load(&store, &resolved.name)?;
            println!("{}", serde_json::to_string_pretty(&metadata)?);
        }
        OutputFormat::Table => {
            eprintln!("VM '{}' ready.", resolved.name);
        }
    }

    Ok(())
}

/// Poll SSH until the VM responds or timeout is reached.
fn wait_for_ssh(store: &StateStore, vm_name: &str, timeout_secs: u64) -> anyhow::Result<()> {
    use std::time::Duration;

    let (metadata, network) = load_running_with_ip(store, vm_name)?;
    let guest_ip = &network.guest_ip;
    let key_path = &metadata.ssh.key;
    let user = &metadata.ssh.user;

    eprint!("Waiting for SSH");
    let rt = tokio::runtime::Runtime::new()?;
    let timeout = Duration::from_secs(timeout_secs);

    match rt.block_on(async {
        ember_core::ssh::client::connect_with_timeout(guest_ip, user, key_path, timeout).await
    }) {
        Ok(client) => {
            rt.block_on(async { client.close().await }).ok();
            eprintln!(" ready.");
            Ok(())
        }
        Err(_) => {
            eprintln!(" timeout ({timeout_secs}s).");
            eprintln!(
                "  hint: VM is running but SSH is slow. Try:\n\
                 \x20   ember exec --wait {timeout_secs} {vm_name} -- echo hello"
            );
            Ok(()) // Non-fatal — VM is running, SSH is just slow
        }
    }
}

/// Post-clone steps: grow disk, inject SSH key, save metadata.
///
/// Separated from [`create`] so the caller can clean up storage on failure.
fn create_post_clone(
    resolved: &ResolvedVmCreate,
    store: &StateStore,
    global_config: &mut GlobalConfig,
    storage: &Storage,
    vm_disk: &str,
    image_size_mib: u64,
    image_ref: &str,
) -> anyhow::Result<()> {
    // Grow disk if requested disk size exceeds image size.
    let requested_size_mib = resolved.disk_size as u64 * 1024;
    let needs_resize = requested_size_mib > image_size_mib;
    if needs_resize {
        eprintln!(
            "Growing disk to {}...",
            format_bytes_binary(resolved.disk_size as u64 * GIB)
        );
        storage.resize(
            &resolved.name,
            ByteSize::from_gib(resolved.disk_size as u64),
        )?;
    }

    // Inject per-VM SSH key into the rootfs image.
    // Linux: mounts the block device, writes the key, unmounts.
    // macOS: uses debugfs to write directly into the ext4 image.
    let dev_path = storage.disk_device_path(&resolved.name);
    let pubkey_path = image::inject::default_ssh_pubkey_path().ok_or_else(|| {
        anyhow::anyhow!(
            "no SSH public key found at ~/.ssh/id_ed25519.pub or ~/.ssh/id_rsa.pub\n\
             Hint: create one with: ssh-keygen -t ed25519"
        )
    })?;
    eprintln!("Injecting SSH key from {}...", pubkey_path.display());
    let detected_ssh_user = storage.inject_ssh_key(&dev_path, &pubkey_path)?;

    // Inject /etc/hosts with the VM hostname so sudo and other tools
    // can resolve the machine's own name without warnings.
    storage.inject_hostname(&dev_path, &resolved.name)?;

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
        disk_path: vm_disk.to_string(),
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
        parent_vm: None,
        vsock: resolved.vsock_info(store)?,
    };

    vm::save(store, &metadata)?;

    eprintln!("VM '{}' created successfully.", resolved.name);

    Ok(())
}

/// Fork an existing VM into a new independent VM.
///
/// Workflow: validate source is stopped → COW clone source disk into
/// new VM → optionally grow disk → resolve kernel → save metadata → optionally start.
///
/// No SSH key injection — the forked disk already has keys from the source VM.
fn fork(args: &ForkArgs, state_dir: &Path) -> anyhow::Result<()> {
    use ember_core::cleanup::Rollback;

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
                    "cannot shrink disk: requested {} but source is {}",
                    format_bytes_binary(gib as u64 * GIB),
                    format_bytes_binary(source.disk_size_gib as u64 * GIB)
                );
            }
            gib
        }
        None => source.disk_size_gib,
    };

    let subnet = args.network.clone().or(source.subnet.clone());

    let storage = Storage::new(&global_config);

    // Clone source VM's storage into the new VM via the storage backend.
    println!("Forking '{}' → '{}'...", args.source, args.name);
    let vm_disk_path = storage.clone_vm_storage(&args.source, &args.name)?;
    let vm_disk = vm_disk_path.to_string_lossy().to_string();

    let mut rollback = Rollback::new();
    {
        let storage = storage.clone();
        let parent = args.source.clone();
        let sd = state_dir.to_path_buf();
        let name = args.name.clone();
        rollback.push("fork clone + snapshot", move || {
            let _ = storage.destroy_vm_storage(&name);
            let _ = storage.cleanup_fork(&parent, &name);
            let _ = vm::delete(&StateStore::new(sd), &name);
        });
    }

    // Grow disk if requested.
    let needs_resize = disk_size_gib > source.disk_size_gib;
    if needs_resize {
        println!(
            "Growing disk to {}...",
            format_bytes_binary(disk_size_gib as u64 * GIB)
        );
        storage.resize(&args.name, ByteSize::from_gib(disk_size_gib as u64))?;
    }

    // Inject /etc/hosts with the new VM's hostname (the cloned disk
    // still has the source VM's hostname from its creation).
    let dev_path = storage.disk_device_path(&args.name);
    storage.inject_hostname(&dev_path, &args.name)?;

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
        disk_path: vm_disk,
        boot_args: source.boot_args.clone(),
        subnet,
        network: None,
        pid: None,
        api_socket: store.vm_dir(&args.name).join("firecracker.sock"),
        created_at: vm::now_iso8601(),
        ssh: source.ssh.clone(),
        parent_vm: Some(args.source.clone()),
        vsock: if args.vsock || source.vsock.is_some() {
            let uds_path = store.vm_dir(&args.name).join("vsock.sock");
            validate_uds_path(&uds_path)?;
            let cid = ember_core::state::vsock::allocate(&store, &args.name)?;
            Some(vm::VsockInfo {
                uds_path,
                guest_cid: cid,
            })
        } else {
            None
        },
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

/// Start a VM: set up networking, spawn the hypervisor, boot.
///
/// Linux: allocate IP → create TAP device → set iptables → spawn Firecracker
/// → configure via API → start instance.
/// macOS: spawn ember-vz → wait for ready signal → discover guest IP via DHCP.
/// Both: update metadata with running state.
///
/// Uses a [`Rollback`] guard to ensure all resources (IP allocation, TAP device,
/// iptables rules, Firecracker process) are cleaned up if any step fails.
fn start(args: &StartArgs, state_dir: &Path) -> anyhow::Result<()> {
    use ember_core::cleanup::Rollback;

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
    eprintln!("Setting up network...");
    let net_info = net_backend.setup(&metadata, &config)?;
    eprintln!(
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

    eprintln!("Starting VM...");
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

    // ── Persist state ─────────────────────────────────────────────

    metadata.status = VmStatus::Running;
    metadata.pid = Some(pid);
    vm::save(&store, &metadata)?;

    // Everything succeeded — keep all resources.
    rollback.commit();

    eprintln!("VM '{}' started (pid {}).", args.name, pid);
    Ok(())
}

/// Stop a running VM: graceful shutdown via SSH poweroff, then SIGKILL fallback.
///
/// Workflow: validate state → SSH poweroff (or SendCtrlAltDel fallback, or skip
/// if --force) → wait for exit → SIGKILL if still alive → clean up network +
/// socket → update metadata.
fn stop(args: &StopArgs, state_dir: &Path) -> anyhow::Result<()> {
    if args.all {
        return stop_all(args.force, state_dir);
    }

    let name = args.name.as_deref().unwrap();
    let store = StateStore::new(state_dir.to_path_buf());

    // Load and validate VM state.
    let mut metadata = vm::load(&store, name)?;
    match metadata.status {
        VmStatus::Running | VmStatus::Paused => {}
        _ => {
            return Err(Error::VmWrongState {
                name: name.to_string(),
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
            name,
            metadata.status,
            name
        )
    })?;

    // Stop the VM via the backend.
    if !Vm::is_running(pid) {
        println!("VM process (pid {pid}) is already dead.");
    } else if args.force {
        println!("Force-stopping VM (pid {pid})...");
        Vm::force_stop(&metadata)?;
    } else {
        println!("Stopping VM '{}'...", name);
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

    println!("VM '{}' stopped.", name);
    Ok(())
}

/// Stop all running/paused VMs.
fn stop_all(force: bool, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let vms = vm::list(&store)?;
    let targets: Vec<_> = vms
        .iter()
        .filter(|v| matches!(v.status, VmStatus::Running | VmStatus::Paused))
        .collect();

    if targets.is_empty() {
        println!("No running VMs to stop.");
        return Ok(());
    }

    println!("Stopping {} VMs...", targets.len());
    for metadata in &targets {
        let stop_args = StopArgs {
            name: Some(metadata.name.clone()),
            all: false,
            force,
        };
        if let Err(e) = stop(&stop_args, state_dir) {
            eprintln!("warning: failed to stop '{}': {}", metadata.name, e);
        }
    }

    Ok(())
}

/// Pause a running VM via the hypervisor backend.
///
/// Workflow: validate VM is running → pause via backend → update metadata.
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

    // Platform-specific pre-pause validation (e.g. API socket check on Linux).
    CurrentPlatform::pre_pause_check(&metadata)?;

    println!("Pausing VM '{}'...", args.name);
    Vm::pause(&metadata)?;

    metadata.status = VmStatus::Paused;
    vm::save(&store, &metadata)?;

    println!("VM '{}' paused.", args.name);
    Ok(())
}

/// Resume a paused VM via the hypervisor backend.
///
/// Workflow: validate VM is paused → resume via backend → update metadata.
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

    // Platform-specific pre-resume validation (e.g. API socket check on Linux).
    CurrentPlatform::pre_pause_check(&metadata)?;

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
/// → grow disk → expand ext4 → update metadata.
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
            "new disk size ({}) must be larger than current size ({})",
            format_bytes_binary(new_gib as u64 * GIB),
            format_bytes_binary(current_gib as u64 * GIB)
        );
    }

    // Grow the disk via the storage backend (handles resize + ext4 expand).
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);
    println!(
        "Resizing disk to {}...",
        format_bytes_binary(new_gib as u64 * GIB)
    );
    storage.resize(&args.name, args.disk_size)?;

    // Update metadata.
    metadata.disk_size_gib = new_gib;
    vm::save(&store, &metadata)?;

    println!(
        "VM '{}' disk resized from {} to {}.",
        args.name,
        format_bytes_binary(current_gib as u64 * GIB),
        format_bytes_binary(new_gib as u64 * GIB)
    );
    Ok(())
}

/// Update a stopped VM's configuration.
///
/// Modifies metadata fields that are only read at boot time (cpus, memory,
/// kernel, boot args) or at SSH connect time (ssh user/key). Requires the
/// VM to be stopped.
fn update_config(args: &UpdateConfigArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let mut metadata = vm::require_stopped(&store, &args.name, "updating configuration")?;

    // Require at least one field to update.
    if args.cpus.is_none()
        && args.memory.is_none()
        && args.kernel.is_none()
        && args.boot_args.is_none()
        && args.ssh_user.is_none()
        && args.ssh_key.is_none()
    {
        anyhow::bail!("no configuration changes specified");
    }

    let mut changes = Vec::new();

    if let Some(cpus) = args.cpus {
        if cpus == 0 {
            anyhow::bail!("cpus must be at least 1");
        }
        metadata.cpus = cpus;
        changes.push(format!("cpus: {cpus}"));
    }

    if let Some(ref memory) = args.memory {
        let mib = memory
            .to_mib()
            .map_err(|e| anyhow::anyhow!("invalid memory size: {e}"))?;
        metadata.memory_mib = mib;
        changes.push(format!("memory: {}", format_bytes_binary(mib as u64 * MIB)));
    }

    if let Some(ref kernel) = args.kernel {
        let mut config: GlobalConfig = store.read(&store.config_path())?;
        let kernel_path = ensure_kernel(&Some(kernel.clone()), &mut config, &store)?;
        metadata.kernel_path = kernel_path.clone();
        changes.push(format!("kernel: {}", kernel_path.display()));
    }

    if let Some(ref boot_args) = args.boot_args {
        if boot_args.is_empty() {
            metadata.boot_args = None;
            changes.push("boot-args: cleared".to_string());
        } else {
            metadata.boot_args = Some(boot_args.clone());
            changes.push(format!("boot-args: {boot_args}"));
        }
    }

    if let Some(ref user) = args.ssh_user {
        metadata.ssh.user = user.clone();
        changes.push(format!("ssh-user: {user}"));
    }

    if let Some(ref key) = args.ssh_key {
        let expanded = config::vm::expand_tilde(key);
        metadata.ssh.key = expanded.clone();
        changes.push(format!("ssh-key: {}", expanded.display()));
    }

    vm::save(&store, &metadata)?;

    println!("Updated VM '{}':", args.name);
    for change in &changes {
        println!("  {change}");
    }
    Ok(())
}

/// Delete a VM and all its resources.
///
/// Workflow: force-stop if running (requires --force) → clean up network →
/// destroy storage (recursively, including user snapshots) → remove state
/// directory.
///
/// Each cleanup step is idempotent — continues if the resource is already gone.
fn delete(args: &DeleteArgs, state_dir: &Path) -> anyhow::Result<()> {
    if args.all {
        return delete_all(args.force, state_dir);
    }

    let name = args.name.as_deref().unwrap();
    let store = StateStore::new(state_dir.to_path_buf());

    // Load VM metadata (must exist).
    let metadata = vm::load(&store, name)?;

    // If the VM is running or paused, require --force.
    if matches!(metadata.status, VmStatus::Running | VmStatus::Paused) && !args.force {
        anyhow::bail!(
            "vm '{}' is {} — stop it first or use --force",
            name,
            metadata.status
        );
    }

    // Check for storage-level dependents (e.g. ZFS fork snapshots with clones).
    // On macOS/APFS this always returns empty — forks are independent.
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);
    let dependents = storage.storage_dependents(name)?;
    if !dependents.is_empty() {
        if !args.force {
            anyhow::bail!(
                "vm '{}' has dependent forks: {}\n\
                 Delete them first, or use --force to cascade-delete all dependents.",
                name,
                dependents.join(", ")
            );
        }
        // --force: cascade-delete all dependent VMs first.
        for dep_name in &dependents {
            if let Ok(dep_meta) = vm::load(&store, dep_name) {
                println!("Cascade-deleting dependent VM '{dep_name}'...");
                force_delete_vm(&store, &dep_meta)?;
            }
        }
    }

    force_delete_vm(&store, &metadata)?;
    Ok(())
}

/// Delete all VMs.
fn delete_all(force: bool, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let vms = vm::list(&store)?;

    if vms.is_empty() {
        println!("No VMs to delete.");
        return Ok(());
    }

    if !force {
        let running = vms
            .iter()
            .any(|v| matches!(v.status, VmStatus::Running | VmStatus::Paused));
        if running {
            anyhow::bail!("some VMs are still running — use --force to stop and delete them");
        }
    }

    println!("Deleting {} VMs...", vms.len());
    for metadata in &vms {
        let delete_args = DeleteArgs {
            name: Some(metadata.name.clone()),
            all: false,
            force,
        };
        if let Err(e) = delete(&delete_args, state_dir) {
            eprintln!("warning: failed to delete '{}': {}", metadata.name, e);
        }
    }

    Ok(())
}

/// Force-delete a VM: kill process, clean up network, destroy storage, remove state.
///
/// Idempotent — each cleanup step continues if the resource is already gone.
/// Called from `vm delete --force` and `image delete --force`.
pub fn force_delete_vm(store: &StateStore, metadata: &VmMetadata) -> anyhow::Result<()> {
    // Recursively delete any VMs forked from this one first.
    // On ZFS, fork children hold clone references to this VM's fork snapshots,
    // preventing `zfs destroy -r` from succeeding on this VM.
    let fork_children: Vec<VmMetadata> = vm::list(store)?
        .into_iter()
        .filter(|v| v.parent_vm.as_deref() == Some(&metadata.name))
        .collect();

    for child in &fork_children {
        println!("Deleting forked VM '{}'...", child.name);
        force_delete_vm(store, child)?;
    }

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

    // Release vsock CID if one was allocated.
    if metadata.vsock.is_some() {
        let _ = ember_core::state::vsock::release(store, &metadata.name);
    }

    // Clean up networking via the backend.
    let net_backend = Network::new(store.clone());
    let _ = net_backend.teardown(metadata);

    // Platform-specific post-delete cleanup (e.g. udevadm settle on Linux).
    CurrentPlatform::post_delete_cleanup();

    // Destroy storage via the backend.
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);

    println!("Destroying storage for VM '{}'...", metadata.name);
    let _ = storage.destroy_vm_storage(&metadata.name);

    // Clean up fork-related resources on the parent VM (e.g. ZFS snapshot).
    // No-op on macOS/APFS where forks are independent.
    if let Some(ref parent) = metadata.parent_vm {
        let _ = storage.cleanup_fork(parent, &metadata.name);
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
                "{:<20} {:<10} {:<16} {:<6} {:>4} {:>8} {:>8}",
                "NAME", "STATUS", "IP", "VSOCK", "CPUS", "MEM", "DISK"
            );
            for vm in &vms {
                let ip = vm
                    .network
                    .as_ref()
                    .map(|n| n.guest_ip.as_str())
                    .unwrap_or("-");
                let vsock = if vm.vsock.is_some() { "✓" } else { "-" };
                println!(
                    "{:<20} {:<10} {:<16} {:<6} {:>4} {:>8} {:>8}",
                    vm.name,
                    vm.status,
                    ip,
                    vsock,
                    vm.cpus,
                    format_bytes_binary(vm.memory_mib as u64 * MIB),
                    format_bytes_binary(vm.disk_size_gib as u64 * GIB),
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
            println!(
                "Memory:      {}",
                format_bytes_binary(metadata.memory_mib as u64 * MIB)
            );
            println!(
                "Disk:        {}",
                format_bytes_binary(metadata.disk_size_gib as u64 * GIB)
            );
            println!("Kernel:      {}", metadata.kernel_path.display());
            for (label, value) in CurrentPlatform::inspect_vm_extra(&metadata) {
                println!("{:<13}{}", label, value);
            }
            println!("Created:     {}", metadata.created_at);

            if let Some(pid) = metadata.pid {
                println!("PID:         {}", pid);
            }

            if let Some(ref net) = metadata.network {
                println!("Network:");
                if !net.tap_device.is_empty() {
                    println!("  TAP device:  {}", net.tap_device);
                }
                println!("  Host IP:     {}", net.host_ip);
                println!("  Guest IP:    {}", net.guest_ip);
                println!("  Netmask:     {}", net.netmask);
                if let Some(ref mac) = net.guest_mac {
                    println!("  Guest MAC:   {}", mac);
                }
            }

            if let Some(ref vsock) = metadata.vsock {
                println!("Vsock:");
                println!("  UDS path:    {}", vsock.uds_path.display());
                println!("  Guest CID:   {}", vsock.guest_cid);
            }

            println!("SSH:");
            println!("  User:        {}", metadata.ssh.user);
            println!("  Key:         {}", metadata.ssh.key.display());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_uds_path_short_ok() {
        let path = Path::new("/tmp/ember/vms/myvm/vsock.sock");
        assert!(validate_uds_path(path).is_ok());
    }

    #[test]
    fn validate_uds_path_at_limit_fails() {
        // Build a path exactly at the limit (104 bytes).
        let long_name = "x".repeat(MAX_UDS_PATH_LEN);
        let path = PathBuf::from(long_name);
        assert!(validate_uds_path(&path).is_err());
    }

    #[test]
    fn validate_uds_path_over_limit_fails() {
        let long_name = "x".repeat(MAX_UDS_PATH_LEN + 50);
        let path = PathBuf::from(long_name);
        let err = validate_uds_path(&path).unwrap_err();
        assert!(
            err.to_string().contains("too long"),
            "error should mention 'too long': {err}"
        );
    }
}
