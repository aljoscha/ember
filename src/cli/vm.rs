use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use uuid::Uuid;

use super::fmt::{format_bytes_binary, print_table, Align, GIB, MIB};
use crate::backend::{
    create_storage, CurrentPlatform, Network, NetworkBackend, Platform, Storage, Vm, VmBackend,
    VolumeHandle,
};
use crate::image;
use ember_core::config;
use ember_core::config::size::ByteSize;
use ember_core::config::GlobalConfig;
use ember_core::error::Error;
use ember_core::image::registry::ImageRegistry;
use ember_core::state::store::StateStore;
use ember_core::state::vm::{self, NetworkInfo, SshConfig, VmMetadata, VmStatus};

/// Build a placeholder [`VmMetadata`] from a freshly returned
/// [`VolumeHandle`].
///
/// Used between `clone_for_vm`/`clone_vm_storage` and the moment the
/// fully populated metadata is constructed: the storage backend reads
/// `name`, `disk_path`, and `thin_id` from this stub for resize, mount,
/// and SSH-key injection. All other fields are placeholders inherited
/// from [`VmMetadata::default_for_teardown`].
fn pending_metadata(name: &str, handle: &VolumeHandle, disk_size_gib: u32) -> VmMetadata {
    let mut m = VmMetadata::default_for_teardown();
    m.name = name.to_string();
    m.disk_path = handle.disk_path.to_string_lossy().into_owned();
    m.thin_id = handle.thin_id;
    // dm-thin needs the size to (re)activate the thin device. Stash the
    // requested disk size so `disk_device_path(pending)` can re-attach
    // post-resize even if the kernel state was somehow torn down.
    m.disk_size_gib = disk_size_gib;
    m
}

/// Build a placeholder [`VmMetadata`] when only the name is available
/// (e.g., recovery paths where the real record can no longer be loaded).
fn name_only_metadata(name: &str) -> VmMetadata {
    let mut m = VmMetadata::default_for_teardown();
    m.name = name.to_string();
    m
}

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
    #[command(visible_alias = "delete")]
    Rm(RmArgs),

    /// List all VMs
    List(ListArgs),

    /// Show detailed VM information
    Inspect(InspectArgs),

    /// Copy a VM into a new independent VM
    #[command(visible_alias = "fork")]
    Cp(CpArgs),

    /// Rename a stopped VM
    Rename(RenameArgs),
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
pub struct RmArgs {
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

    /// Only show VMs in the given state (repeat to include several).
    /// Omit to show all VMs.
    #[arg(long)]
    pub status: Vec<StatusFilter>,
}

#[derive(Args)]
pub struct CpArgs {
    /// Source VM to copy from
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

    /// Don't start the VM after copying
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Args)]
pub struct RenameArgs {
    /// Current VM name
    pub name: String,

    /// New VM name
    pub new_name: String,
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

/// CLI mirror of [`VmStatus`] used for `vm list --status` filtering.
///
/// Kept separate from the core type so `clap` stays out of `ember-core`,
/// mirroring how [`OutputFormat`] lives in the CLI layer.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StatusFilter {
    Created,
    Running,
    Stopped,
    Paused,
}

impl StatusFilter {
    fn matches(self, status: VmStatus) -> bool {
        matches!(
            (self, status),
            (StatusFilter::Created, VmStatus::Created)
                | (StatusFilter::Running, VmStatus::Running)
                | (StatusFilter::Stopped, VmStatus::Stopped)
                | (StatusFilter::Paused, VmStatus::Paused)
        )
    }
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
        VmCommand::Rm(args) => rm(args, state_dir),
        VmCommand::List(args) => list(args, state_dir),
        VmCommand::Inspect(args) => inspect(args, state_dir),
        VmCommand::Cp(args) => cp(args, state_dir),
        VmCommand::Rename(args) => rename(args, state_dir),
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

    let image_ref = image_entry.reference.clone();
    let image_size_mib = image_entry.size_mib;

    let storage = create_storage(&global_config);

    let mut rollback = Rollback::new();

    // Clone base image → per-VM disk (instant, copy-on-write).
    println!("Cloning image for VM '{}'...", resolved.name);
    let handle = storage.clone_for_vm(image_entry, &resolved.name)?;
    let pending = pending_metadata(&resolved.name, &handle, resolved.disk_size);
    {
        let storage = storage.clone();
        let sd = state_dir.to_path_buf();
        let pending = pending.clone();
        rollback.push("VM storage clone", move || {
            let _ = storage.destroy_vm_storage(&pending);
            let _ = vm::delete(&StateStore::new(sd), &pending.name);
        });
    }

    create_post_clone(
        &resolved,
        &store,
        &mut global_config,
        &storage,
        &pending,
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
/// `pending` is the in-progress VM metadata (built from the [`VolumeHandle`]
/// returned by `clone_for_vm`); the storage backend reads `name`, `disk_path`,
/// and `thin_id` from it for the resize/inject calls below.
fn create_post_clone(
    resolved: &ResolvedVmCreate,
    store: &StateStore,
    global_config: &mut GlobalConfig,
    storage: &Storage,
    pending: &VmMetadata,
    image_size_mib: u64,
    image_ref: &str,
) -> anyhow::Result<()> {
    // Grow disk if requested disk size exceeds image size.
    let requested_size_mib = resolved.disk_size as u64 * 1024;
    let needs_resize = requested_size_mib > image_size_mib;
    if needs_resize {
        println!(
            "Growing disk to {}...",
            format_bytes_binary(resolved.disk_size as u64 * GIB)
        );
        storage.resize(pending, ByteSize::from_gib(resolved.disk_size as u64))?;
    }

    // Inject per-VM SSH key into the rootfs image.
    // Linux: mounts the block device, writes the key, unmounts.
    // macOS: uses debugfs to write directly into the ext4 image.
    let dev_path = storage.disk_device_path(pending)?;
    let pubkey_path = image::inject::default_ssh_pubkey_path().ok_or_else(|| {
        anyhow::anyhow!(
            "no SSH public key found at ~/.ssh/id_ed25519.pub or ~/.ssh/id_rsa.pub\n\
             Hint: create one with: ssh-keygen -t ed25519"
        )
    })?;
    println!("Injecting SSH key from {}...", pubkey_path.display());
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

    // Build and save VM metadata. The disk path and thin_id come from
    // the pending stub built right after `clone_for_vm` returned.
    let metadata = VmMetadata {
        name: resolved.name.clone(),
        id: Uuid::new_v4(),
        status: VmStatus::Created,
        image: image_ref.to_string(),
        cpus: resolved.cpus,
        memory_mib: resolved.memory,
        disk_size_gib: resolved.disk_size,
        kernel_path,
        disk_path: pending.disk_path.clone(),
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
        thin_id: pending.thin_id,
    };

    vm::save(store, &metadata)?;

    println!("VM '{}' created successfully.", resolved.name);

    Ok(())
}

/// Copy an existing VM into a new independent VM.
///
/// Workflow: validate source is stopped → COW clone source disk into
/// new VM → optionally grow disk → resolve kernel → save metadata → optionally start.
///
/// No SSH key injection — the cloned disk already has keys from the source VM.
fn cp(args: &CpArgs, state_dir: &Path) -> anyhow::Result<()> {
    use ember_core::cleanup::Rollback;

    let store = StateStore::new(state_dir.to_path_buf());
    let mut global_config: GlobalConfig = store.read(&store.config_path())?;

    // Source must exist and be stopped.
    let source = vm::require_stopped(&store, &args.source, "copying")?;

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

    let storage = create_storage(&global_config);

    // Clone source VM's storage into the new VM via the storage backend.
    println!("Copying '{}' → '{}'...", args.source, args.name);
    let handle = storage.clone_vm_storage(&source, &args.name)?;
    let pending = pending_metadata(&args.name, &handle, disk_size_gib);

    let mut rollback = Rollback::new();
    {
        let storage = storage.clone();
        let parent = source.clone();
        let pending = pending.clone();
        let sd = state_dir.to_path_buf();
        rollback.push("vm copy clone + snapshot", move || {
            let _ = storage.destroy_vm_storage(&pending);
            let _ = storage.cleanup_fork(&parent, &pending);
            let _ = vm::delete(&StateStore::new(sd), &pending.name);
        });
    }

    // Grow disk if requested.
    let needs_resize = disk_size_gib > source.disk_size_gib;
    if needs_resize {
        println!(
            "Growing disk to {}...",
            format_bytes_binary(disk_size_gib as u64 * GIB)
        );
        storage.resize(&pending, ByteSize::from_gib(disk_size_gib as u64))?;
    }

    // Inject /etc/hosts with the new VM's hostname (the cloned disk
    // still has the source VM's hostname from its creation).
    let dev_path = storage.disk_device_path(&pending)?;
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
        disk_path: pending.disk_path.clone(),
        boot_args: source.boot_args.clone(),
        subnet,
        network: None,
        pid: None,
        api_socket: store.vm_dir(&args.name).join("firecracker.sock"),
        created_at: vm::now_iso8601(),
        ssh: source.ssh.clone(),
        parent_vm: Some(args.source.clone()),
        thin_id: pending.thin_id,
    };

    vm::save(&store, &metadata)?;

    rollback.commit();

    println!("VM '{}' copied from '{}'.", args.name, args.source);

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

/// Rename a stopped VM.
///
/// Renames the storage volume, moves the per-VM state directory, and
/// updates `vm.json`. Any forked children's `parent_vm` field is
/// rewritten to the new name so the fork-dependency graph stays
/// consistent.
fn rename(args: &RenameArgs, state_dir: &Path) -> anyhow::Result<()> {
    if args.name == args.new_name {
        anyhow::bail!("new name is the same as the current name");
    }
    if args.new_name.is_empty() {
        anyhow::bail!("new name must not be empty");
    }

    let store = StateStore::new(state_dir.to_path_buf());

    // Source must exist and be stopped (renaming touches paths the
    // hypervisor has open while running).
    let mut metadata = vm::require_stopped(&store, &args.name, "renaming")?;

    // Target must not exist.
    if vm::exists(&store, &args.new_name) {
        anyhow::bail!("vm '{}' already exists", args.new_name);
    }

    // Rename the storage volume first — if this fails, nothing else
    // has moved yet.
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = create_storage(&config);
    let new_handle = storage.rename_vm_storage(&metadata, &args.new_name)?;

    // Move the per-VM state directory. If this fails, roll the
    // storage rename back so the filesystem and the kernel agree.
    let old_dir = store.vm_dir(&args.name);
    let new_dir = store.vm_dir(&args.new_name);
    if let Err(e) = std::fs::rename(&old_dir, &new_dir) {
        let revert = VmMetadata {
            name: args.new_name.clone(),
            disk_path: new_handle.disk_path.to_string_lossy().into_owned(),
            thin_id: new_handle.thin_id,
            ..VmMetadata::default_for_teardown()
        };
        let _ = storage.rename_vm_storage(&revert, &args.name);
        return Err(anyhow::anyhow!(
            "failed to move state dir {} → {}: {e}",
            old_dir.display(),
            new_dir.display()
        ));
    }

    // Update metadata to point at the new name/paths and persist.
    metadata.name = args.new_name.clone();
    metadata.disk_path = new_handle.disk_path.to_string_lossy().into_owned();
    metadata.thin_id = new_handle.thin_id;
    metadata.api_socket = store.vm_dir(&args.new_name).join("firecracker.sock");
    vm::save(&store, &metadata)?;

    // Rewrite `parent_vm` on any forked children so the dependency
    // graph keeps resolving to a real VM.
    for mut child in vm::list(&store)? {
        if child.parent_vm.as_deref() == Some(&args.name) {
            child.parent_vm = Some(args.new_name.clone());
            vm::save(&store, &child)?;
        }
    }

    println!("VM '{}' renamed to '{}'.", args.name, args.new_name);
    Ok(())
}

/// Maximum fraction of host RAM the sum of all running VMs is allowed
/// to reserve. The kernel OOM-kills at 100%; we leave ~20% for the host
/// itself (kernel, page cache, the user's shells/editor/browser) plus
/// VM-side overhead that isn't billed against `memory_mib` (firecracker
/// RSS, KVM/EPT slab, virtio buffers).
const MAX_HOST_RAM_FRACTION: f64 = 0.80;

/// Refuse a `vm start` that would push committed guest RAM past
/// [`MAX_HOST_RAM_FRACTION`] of host RAM. This is the only thing
/// standing between the user and "I forgot how big my VMs already are";
/// firecracker itself has no notion of host capacity.
fn check_admission(store: &StateStore, vm_name: &str, requested_mib: u32) -> anyhow::Result<()> {
    let host_mib = match CurrentPlatform::host_ram_mib() {
        Ok(m) => m,
        Err(e) => {
            // Don't block on inability to read host RAM — fall back to
            // pre-admission behavior (let the kernel be the backstop).
            eprintln!("warning: skipping admission check: {e}");
            return Ok(());
        }
    };
    let committed_mib: u32 = vm::list(store)?
        .iter()
        .filter(|v| matches!(v.status, VmStatus::Running | VmStatus::Paused))
        .map(|v| v.memory_mib)
        .sum();
    let new_total = committed_mib + requested_mib;
    let cap = (host_mib as f64 * MAX_HOST_RAM_FRACTION) as u32;
    if new_total > cap {
        anyhow::bail!(
            "starting '{vm_name}' ({requested_mib} MiB) would commit {new_total} MiB of guest RAM, \
             exceeding the {pct}% cap of {cap} MiB on a host with {host_mib} MiB total \
             ({committed_mib} MiB already committed by running VMs). \
             Stop another VM first, or override per-VM memory with 'ember vm update-config'.",
            pct = (MAX_HOST_RAM_FRACTION * 100.0) as u32,
        );
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

    check_admission(&store, &metadata.name, metadata.memory_mib)?;

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
        let config_clone = config.clone();
        rollback.push("network", move || {
            let teardown_meta = VmMetadata {
                name: meta_name,
                network: Some(net_info_clone),
                ..VmMetadata::default_for_teardown()
            };
            let _ = net.teardown(&teardown_meta, &config_clone);
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
    let config: GlobalConfig = store.read(&store.config_path())?;
    let _ = net_backend.teardown(&metadata, &config);

    // Update metadata.
    metadata.status = VmStatus::Stopped;
    metadata.pid = None;
    metadata.network = None;
    vm::save(&store, &metadata)?;

    println!("VM '{}' stopped.", args.name);
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
    let storage = create_storage(&config);
    println!(
        "Resizing disk to {}...",
        format_bytes_binary(new_gib as u64 * GIB)
    );
    storage.resize(&metadata, args.disk_size)?;

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
fn rm(args: &RmArgs, state_dir: &Path) -> anyhow::Result<()> {
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

    // Check for storage-level dependents (e.g. ZFS fork snapshots with clones).
    // On macOS/APFS this always returns empty — forks are independent.
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = create_storage(&config);
    let dependents = storage.storage_dependents(&metadata)?;
    if !dependents.is_empty() {
        if !args.force {
            anyhow::bail!(
                "vm '{}' has dependent forks: {}\n\
                 Delete them first, or use --force to cascade-delete all dependents.",
                args.name,
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

    // Read the persisted config once and reuse it for both network
    // teardown (needs the per-installation iptables comment) and
    // storage teardown.
    let config: GlobalConfig = store.read(&store.config_path())?;

    // Clean up networking via the backend.
    let net_backend = Network::new(store.clone());
    let _ = net_backend.teardown(metadata, &config);

    // Platform-specific post-delete cleanup (e.g. udevadm settle on Linux).
    CurrentPlatform::post_delete_cleanup();

    // Destroy storage via the backend.
    let storage = create_storage(&config);

    println!("Destroying storage for VM '{}'...", metadata.name);
    let _ = storage.destroy_vm_storage(metadata);

    // Clean up fork-related resources on the parent VM (e.g. ZFS snapshot).
    // No-op on macOS/APFS where forks are independent.
    if let Some(ref parent_name) = metadata.parent_vm {
        // Use the parent's stored metadata if available; fall back to a
        // name-only stub when the parent record is gone (e.g. cascade
        // cleanup running in the wrong order).
        let parent_md =
            vm::load(store, parent_name).unwrap_or_else(|_| name_only_metadata(parent_name));
        let _ = storage.cleanup_fork(&parent_md, metadata);
    }

    // Remove the VM state directory.
    vm::delete(store, &metadata.name)?;

    println!("VM '{}' deleted.", metadata.name);
    Ok(())
}

/// List all VMs with summary information.
fn list(args: &ListArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let mut vms = vm::list(&store)?;

    // Empty `--status` means "all"; otherwise keep VMs matching any given state.
    if !args.status.is_empty() {
        vms.retain(|vm| args.status.iter().any(|f| f.matches(vm.status)));
    }

    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&vms)?);
        }
        OutputFormat::Table => {
            if vms.is_empty() {
                if args.status.is_empty() {
                    println!(
                        "No VMs found. Create one with: ember vm create <name> --image <image>"
                    );
                } else {
                    println!("No VMs match the requested status.");
                }
                return Ok(());
            }

            let rows: Vec<Vec<String>> = vms
                .iter()
                .map(|vm| {
                    vec![
                        vm.name.clone(),
                        vm.status.to_string(),
                        vm.image.clone(),
                        vm.cpus.to_string(),
                        format_bytes_binary(vm.memory_mib as u64 * MIB),
                        format_bytes_binary(vm.disk_size_gib as u64 * GIB),
                    ]
                })
                .collect();
            print_table(
                &["NAME", "STATUS", "IMAGE", "CPUS", "MEM", "DISK"],
                &[
                    Align::Left,
                    Align::Left,
                    Align::Left,
                    Align::Right,
                    Align::Right,
                    Align::Right,
                ],
                &rows,
            );
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

            println!("SSH:");
            println!("  User:        {}", metadata.ssh.user);
            println!("  Key:         {}", metadata.ssh.key.display());
        }
    }

    Ok(())
}
