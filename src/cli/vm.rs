use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use clap::{Args, Subcommand};
use uuid::Uuid;

use super::init::GlobalConfig;
use crate::error::Error;
use crate::firecracker;
use crate::image;
use crate::image::registry::ImageRegistry;
use crate::state::store::StateStore;
use crate::state::vm::{self, SshConfig, VmMetadata, VmStatus};
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
    #[arg(long)]
    pub image: String,

    /// Number of vCPUs
    #[arg(long, default_value = "1")]
    pub cpus: u32,

    /// Memory in MiB
    #[arg(long, default_value = "128")]
    pub memory: u32,

    /// Disk size in GiB
    #[arg(long, default_value = "1")]
    pub disk_size: u32,

    /// Path to custom kernel
    #[arg(long)]
    pub kernel: Option<PathBuf>,

    /// Network subnet
    #[arg(long)]
    pub network: Option<String>,

    /// VM config YAML file
    #[arg(long = "vm-config")]
    pub config: Option<PathBuf>,

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
        VmCommand::Pause(_) => anyhow::bail!("ember vm pause is not yet implemented"),
        VmCommand::Resume(_) => anyhow::bail!("ember vm resume is not yet implemented"),
        VmCommand::Delete(_) => anyhow::bail!("ember vm delete is not yet implemented"),
        VmCommand::List(_) => anyhow::bail!("ember vm list is not yet implemented"),
        VmCommand::Inspect(_) => anyhow::bail!("ember vm inspect is not yet implemented"),
        VmCommand::Ssh(_) => anyhow::bail!("ember vm ssh is not yet implemented"),
    }
}

/// Create a new VM from an image.
///
/// Workflow: look up image → ZFS clone @base snapshot → grow zvol if needed
/// → mount zvol → inject per-VM SSH key → unmount → save metadata.
fn create(args: &CreateArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;

    // Check VM doesn't already exist.
    if vm::exists(&store, &args.name) {
        anyhow::bail!("vm '{}' already exists", args.name);
    }

    // Look up image in local registry.
    let registry = ImageRegistry::load(&store)?;
    let image_entry = registry
        .find_by_reference(&args.image)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "image '{}' not found locally — pull it first with: ember image pull {}",
                args.image,
                args.image
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
    let base_dataset = format!("{}/{}", config.pool, config.dataset);
    let vm_zvol = format!("{base_dataset}/vms/{}", args.name);

    // Clone @base snapshot → per-VM zvol (instant, copy-on-write).
    let snapshot = format!("{image_zvol}@base");
    println!("Cloning {} → {}...", snapshot, vm_zvol);
    zfs::volume::clone(&snapshot, &vm_zvol)?;

    // Run the remainder in a closure so we can clean up the zvol on failure.
    let result = create_post_clone(args, &store, &config, &vm_zvol, image_size_mib, &image_ref);

    if result.is_err() {
        eprintln!("VM creation failed, cleaning up...");
        let _ = zfs::volume::destroy(&vm_zvol, true);
        let _ = vm::delete(&store, &args.name);
    }

    result
}

/// Post-clone steps: grow zvol, inject SSH key, save metadata.
///
/// Separated from [`create`] so the caller can clean up the zvol on failure.
fn create_post_clone(
    args: &CreateArgs,
    store: &StateStore,
    config: &GlobalConfig,
    vm_zvol: &str,
    image_size_mib: u64,
    image_ref: &str,
) -> anyhow::Result<()> {
    // Grow zvol if requested disk size exceeds image size.
    let requested_size_mib = args.disk_size as u64 * 1024;
    let needs_resize = requested_size_mib > image_size_mib;
    if needs_resize {
        println!("Growing zvol to {} GiB...", args.disk_size);
        zfs::volume::set_volsize(vm_zvol, args.disk_size)?;
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
    inject_result?;
    umount_result?;

    // Determine kernel path.
    let kernel_path = args
        .kernel
        .clone()
        .or(config.kernel_path.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no kernel available — provide --kernel or run 'ember init --kernel-url <url>'"
            )
        })?;

    // Build and save VM metadata.
    let metadata = VmMetadata {
        name: args.name.clone(),
        id: Uuid::new_v4(),
        status: VmStatus::Created,
        image: image_ref.to_string(),
        cpus: args.cpus,
        memory_mib: args.memory,
        disk_size_gib: args.disk_size,
        kernel_path,
        zvol_path: vm_zvol.to_string(),
        network: None,
        pid: None,
        api_socket: store.vm_dir(&args.name).join("firecracker.sock"),
        created_at: vm::now_iso8601(),
        ssh: SshConfig::default(),
    };

    vm::save(store, &metadata)?;

    println!("VM '{}' created successfully.", args.name);

    if !args.no_start {
        println!(
            "Note: auto-start is not yet implemented. Start with: ember vm start {}",
            args.name
        );
    }

    Ok(())
}

/// Start a VM: spawn Firecracker, configure via API, boot.
///
/// Workflow: validate state → clean stale socket → spawn firecracker
/// → wait for API socket → configure machine (CPU, memory, kernel, rootfs)
/// → start instance → update metadata.
///
/// Networking is not yet integrated — the VM boots without network access.
fn start(args: &StartArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

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

    // Build paths.
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

    // Everything from here needs cleanup on failure: kill the process.
    let result = start_configure(&metadata, socket_path, &rootfs_path);
    if let Err(e) = result {
        eprintln!("VM start failed, killing Firecracker process (pid {pid})...");
        let _ = firecracker::process::kill(pid);
        return Err(e);
    }

    // Update metadata.
    metadata.status = VmStatus::Running;
    metadata.pid = Some(pid);
    vm::save(&store, &metadata)?;

    println!("VM '{}' started (pid {}).", args.name, pid);
    Ok(())
}

/// Configure and start a Firecracker instance via the API.
///
/// Runs the async API calls inside a one-shot tokio runtime.
fn start_configure(
    metadata: &VmMetadata,
    socket_path: &Path,
    rootfs_path: &Path,
) -> anyhow::Result<()> {
    // Wait for the API socket to appear.
    firecracker::process::wait_for_socket(socket_path)?;

    // Build VM configuration (no networking yet).
    let vm_config = firecracker::config::VmConfig::new(
        metadata.cpus,
        metadata.memory_mib,
        &metadata.kernel_path,
        rootfs_path,
    );

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
/// → SIGKILL if still alive → clean up socket → update metadata.
///
/// Network cleanup (TAP, iptables, IP release) is not yet implemented.
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
            "vm '{}' is {} but has no PID — state may be corrupted",
            args.name,
            metadata.status
        )
    })?;

    // Check if the process is actually alive.
    if !firecracker::process::is_alive(pid) {
        println!("Firecracker process (pid {pid}) is already dead.");
    } else if args.force {
        // --force: skip graceful shutdown, go straight to SIGKILL.
        println!("Force-killing Firecracker (pid {pid})...");
        firecracker::process::kill(pid)?;
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
        } else {
            // Wait up to 10 seconds for the process to exit.
            println!("Waiting for VM to shut down (up to 10s)...");
            let exited =
                firecracker::process::wait_for_exit(pid, std::time::Duration::from_secs(10));
            if !exited {
                println!("VM did not exit in time, sending SIGKILL...");
                firecracker::process::kill(pid)?;
            }
        }
    }

    // Clean up the API socket.
    let socket_path = &metadata.api_socket;
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }

    // Update metadata.
    metadata.status = VmStatus::Stopped;
    metadata.pid = None;
    vm::save(&store, &metadata)?;

    println!("VM '{}' stopped.", args.name);
    Ok(())
}

/// Inject the invoking user's SSH public key into the rootfs.
fn inject_ssh_key(rootfs_dir: &Path) -> anyhow::Result<()> {
    let pubkey_path = image::inject::default_ssh_pubkey_path().ok_or_else(|| {
        anyhow::anyhow!(
            "no SSH public key found — create one with: ssh-keygen -t ed25519"
        )
    })?;

    println!("Injecting SSH key from {}...", pubkey_path.display());
    image::inject::inject_ssh_authorized_keys(rootfs_dir, &pubkey_path)?;
    Ok(())
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
