use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use clap::{Args, Subcommand};

use super::init::GlobalConfig;
use super::vm::OutputFormat;
use crate::firecracker;
use crate::image;
use crate::image::pull::ImageReference;
use crate::image::registry::{ImageRegistry, new_build_entry, new_entry};
use crate::state::store::StateStore;
use crate::state::vm::{self, VmMetadata, VmStatus};
use crate::zfs;

#[derive(Subcommand)]
pub enum ImageCommand {
    /// Pull an OCI image from a registry
    Pull(PullArgs),

    /// Build a VM image from a Dockerfile
    Build(BuildArgs),

    /// List locally available images
    List(ListArgs),

    /// Delete a local image
    Delete(DeleteArgs),

    /// Show detailed image information
    Inspect(InspectArgs),
}

#[derive(Args)]
pub struct PullArgs {
    /// Image reference (e.g. docker.io/library/ubuntu:22.04)
    pub reference: String,
}

#[derive(Args)]
pub struct BuildArgs {
    /// Image name (e.g. ubuntu-vm, my-image:v1)
    pub name: String,

    /// Path to Dockerfile (default: built-in Ubuntu 24.04 VM image)
    #[arg(long = "file", short = 'f')]
    pub dockerfile: Option<PathBuf>,
}

#[derive(Args)]
pub struct ListArgs {
    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// Image name
    pub name: String,

    /// Force delete, also removing any VMs that depend on this image
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct InspectArgs {
    /// Image reference (e.g. alpine, docker.io/library/alpine:latest)
    pub name: String,

    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

pub fn run(cmd: &ImageCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        ImageCommand::Pull(args) => pull(args, state_dir),
        ImageCommand::Build(args) => build(args, state_dir),
        ImageCommand::List(args) => list(args, state_dir),
        ImageCommand::Delete(args) => delete(args, state_dir),
        ImageCommand::Inspect(args) => inspect(args, state_dir),
    }
}

/// Pull an OCI image from a registry and write it to a ZFS zvol.
///
/// Full pipeline: skopeo pull → inject SSH keys + resolv.conf → ext4 image
/// → zvol create → dd to zvol → @base snapshot → register in local registry.
///
/// Uses a [`Rollback`] guard to ensure the zvol is cleaned up if any step
/// after creation fails (e.g., writing the image or saving the registry).
fn pull(args: &PullArgs, state_dir: &Path) -> anyhow::Result<()> {
    use crate::cleanup::Rollback;

    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let images_dataset = format!("{}/{}/images", config.pool, config.dataset);

    // Parse and validate the image reference.
    let reference = ImageReference::parse(&args.reference)?;
    let local_name = reference.local_name();
    let zvol = format!("{images_dataset}/{local_name}");

    // Check if this image is already pulled.
    let registry = ImageRegistry::load(&store)?;
    if registry.exists(&local_name) {
        println!("Image '{reference}' already exists locally as '{local_name}'.");
        return Ok(());
    }

    println!("Pulling {reference}...");

    // Create a temporary working directory for the pull.
    let work_dir = tempfile::tempdir().map_err(|e| crate::error::Error::Io {
        path: std::env::temp_dir(),
        source: e,
    })?;

    // Step 1: Pull OCI image and unpack layers.
    println!("  Downloading and unpacking layers...");
    let rootfs_dir = image::pull::pull(&reference, work_dir.path())?;

    // Step 2: Inject SSH authorized_keys and resolv.conf into rootfs.
    if let Some(pubkey_path) = image::inject::default_ssh_pubkey_path() {
        if pubkey_path.exists() {
            println!("  Injecting SSH public key from {}...", pubkey_path.display());
            image::inject::inject_ssh_authorized_keys(&rootfs_dir, &pubkey_path)?;
        } else {
            println!(
                "  Warning: SSH public key not found at {}, skipping injection.",
                pubkey_path.display()
            );
        }
    }
    image::inject::inject_resolv_conf(&rootfs_dir)?;
    image::inject::inject_inittab(&rootfs_dir)?;

    // Step 3: Create ext4 filesystem image from rootfs.
    let size_mib = image::ext4::estimate_size_mib(&rootfs_dir)?;
    let ext4_path = work_dir.path().join("rootfs.ext4");
    println!("  Creating ext4 image ({size_mib} MiB)...");
    image::ext4::create(&rootfs_dir, &ext4_path, size_mib)?;

    let mut rollback = Rollback::new();

    // Step 4: Create ZFS zvol and write ext4 image to it.
    println!("  Creating zvol {zvol}...");
    zfs::volume::create(&zvol, size_mib)?;
    {
        let z = zvol.clone();
        rollback.push("ZFS zvol", move || {
            let _ = zfs::volume::destroy(&z, true);
        });
    }

    println!("  Writing image to zvol and creating @base snapshot...");
    image::zvol::write_to_zvol(&ext4_path, &zvol)?;

    // Step 5: Register in local image registry.
    let entry = new_entry(&reference, &zvol, size_mib);
    let mut registry = ImageRegistry::load(&store)?;
    registry.add(entry);
    registry.save(&store)?;

    rollback.commit();

    println!("Image '{reference}' pulled successfully as '{local_name}'.");
    Ok(())
}

/// Build a VM image from a Dockerfile and write it to a ZFS zvol.
///
/// Full pipeline: docker build → export rootfs → inject SSH keys + resolv.conf
/// → ext4 image → zvol create → dd to zvol → @base snapshot → register.
fn build(args: &BuildArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let images_dataset = format!("{}/{}/images", config.pool, config.dataset);

    // Sanitize the name for ZFS dataset use.
    let local_name = image::build::sanitize_name(&args.name)?;
    let zvol = format!("{images_dataset}/{local_name}");

    // Check if this image already exists.
    let registry = ImageRegistry::load(&store)?;
    if registry.exists(&local_name) {
        println!("Image '{}' already exists locally.", local_name);
        return Ok(());
    }

    println!("Building image '{}'...", args.name);

    let work_dir = tempfile::tempdir().map_err(|e| crate::error::Error::Io {
        path: std::env::temp_dir(),
        source: e,
    })?;

    // Resolve the Dockerfile: user-provided or built-in default.
    let dockerfile = match &args.dockerfile {
        Some(path) => {
            if !path.exists() {
                anyhow::bail!("Dockerfile not found: {}", path.display());
            }
            path.clone()
        }
        None => {
            let default_path = work_dir.path().join("Dockerfile");
            std::fs::write(&default_path, image::build::DEFAULT_DOCKERFILE)
                .map_err(|e| crate::error::Error::Io {
                    path: default_path.clone(),
                    source: e,
                })?;
            default_path
        }
    };

    // Step 1: Build container image and export rootfs.
    println!("  Building container image...");
    let rootfs_dir = image::build::build(&dockerfile, work_dir.path(), &local_name)?;

    // Step 2: Inject SSH authorized_keys and resolv.conf into rootfs.
    // Skip inittab — systemd-based images handle init and CtrlAltDel natively.
    if let Some(pubkey_path) = image::inject::default_ssh_pubkey_path() {
        if pubkey_path.exists() {
            println!("  Injecting SSH public key from {}...", pubkey_path.display());
            image::inject::inject_ssh_authorized_keys(&rootfs_dir, &pubkey_path)?;
        } else {
            println!(
                "  Warning: SSH public key not found at {}, skipping injection.",
                pubkey_path.display()
            );
        }
    }
    image::inject::inject_resolv_conf(&rootfs_dir)?;

    // Step 3: Create ext4 filesystem image from rootfs.
    let size_mib = image::ext4::estimate_size_mib(&rootfs_dir)?;
    let ext4_path = work_dir.path().join("rootfs.ext4");
    println!("  Creating ext4 image ({size_mib} MiB)...");
    image::ext4::create(&rootfs_dir, &ext4_path, size_mib)?;

    let mut rollback = crate::cleanup::Rollback::new();

    // Step 4: Create ZFS zvol and write ext4 image to it.
    println!("  Creating zvol {zvol}...");
    zfs::volume::create(&zvol, size_mib)?;
    {
        let z = zvol.clone();
        rollback.push("ZFS zvol", move || {
            let _ = zfs::volume::destroy(&z, true);
        });
    }

    println!("  Writing image to zvol and creating @base snapshot...");
    image::zvol::write_to_zvol(&ext4_path, &zvol)?;

    // Step 5: Register in local image registry.
    let entry = new_build_entry(&args.name, &local_name, &zvol, size_mib);
    let mut registry = ImageRegistry::load(&store)?;
    registry.add(entry);
    registry.save(&store)?;

    rollback.commit();

    println!("Image '{}' built successfully.", local_name);
    Ok(())
}

/// List locally available images.
fn list(args: &ListArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let registry = ImageRegistry::load(&store)?;

    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&registry)?);
        }
        OutputFormat::Table => {
            if registry.is_empty() {
                println!("No images found. Pull one with: ember image pull <reference>");
                return Ok(());
            }

            println!(
                "{:<40} {:<30} {:>8} PULLED",
                "REFERENCE", "LOCAL NAME", "SIZE"
            );
            for img in &registry.images {
                println!(
                    "{:<40} {:<30} {:>5} MiB {}",
                    img.reference, img.local_name, img.size_mib, img.pulled_at
                );
            }
        }
    }

    Ok(())
}

/// Delete a local image: remove from registry and destroy ZFS zvol.
///
/// If VMs were cloned from this image, they hold a ZFS dependency on the
/// image's `@base` snapshot. Without `--force`, the command lists the
/// dependent VMs and exits. With `--force`, it deletes those VMs first,
/// then destroys the image.
fn delete(args: &DeleteArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    // Try parsing as an OCI reference first, fall back to direct local_name
    // lookup for locally built images.
    let local_name = resolve_local_name(&store, &args.name)?;

    // Look up the image entry (don't remove from registry yet — the zvol
    // destroy might fail if there are dependent clones).
    let registry = ImageRegistry::load(&store)?;
    let entry = registry
        .get(&local_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "image '{}' not found locally\n\
                 Hint: run 'ember image list' to see available images",
                args.name
            )
        })?
        .clone();

    // Find VMs that were created from this image.
    let dependent_vms: Vec<VmMetadata> = vm::list(&store)?
        .into_iter()
        .filter(|v| v.image == entry.reference)
        .collect();

    if !dependent_vms.is_empty() {
        let vm_names: Vec<&str> = dependent_vms.iter().map(|v| v.name.as_str()).collect();

        if !args.force {
            anyhow::bail!(
                "image '{}' is in use by VM(s): {}\n\
                 Delete them first, or use --force to delete the image and all dependent VMs.",
                entry.reference,
                vm_names.join(", ")
            );
        }

        // Force-delete each dependent VM.
        for vm_meta in &dependent_vms {
            force_delete_vm(&store, vm_meta)?;
        }
    }

    // Destroy the ZFS zvol (and its @base snapshot) recursively.
    if zfs::volume::exists(&entry.zvol)? {
        println!("Destroying zvol {}...", entry.zvol);
        zfs::volume::destroy(&entry.zvol, true)?;
    }

    // Remove from registry last, after the zvol is gone.
    image::registry::remove_image(&store, &local_name)?;

    println!("Image '{}' deleted.", entry.reference);
    Ok(())
}

/// Force-delete a single VM: stop if running, destroy zvol, remove state.
///
/// Mirrors the logic in `cli/vm.rs delete --force` but is callable from
/// image deletion.
fn force_delete_vm(store: &StateStore, metadata: &VmMetadata) -> anyhow::Result<()> {
    println!("Deleting dependent VM '{}'...", metadata.name);

    // Kill the Firecracker process if the VM is running/paused.
    if matches!(metadata.status, VmStatus::Running | VmStatus::Paused) {
        if let Some(pid) = metadata.pid {
            if firecracker::process::is_alive(pid) {
                println!("  Force-killing Firecracker (pid {pid})...");
                firecracker::process::kill(pid)?;
                firecracker::process::wait_for_exit(pid, std::time::Duration::from_secs(5));
            }
        }
        if metadata.api_socket.exists() {
            let _ = std::fs::remove_file(&metadata.api_socket);
        }
    }

    let _ = ProcessCommand::new("udevadm").arg("settle").status();

    // Destroy the VM's zvol.
    if zfs::volume::exists(&metadata.zvol_path)? {
        println!("  Destroying zvol '{}'...", metadata.zvol_path);
        match zfs::volume::destroy(&metadata.zvol_path, true) {
            Ok(()) => {}
            Err(e) => {
                eprintln!(
                    "  Warning: failed to destroy zvol '{}': {e}",
                    metadata.zvol_path
                );
            }
        }
    }

    // Remove the VM state directory.
    vm::delete(store, &metadata.name)?;

    println!("  VM '{}' deleted.", metadata.name);
    Ok(())
}

/// Show detailed information about a local image.
fn inspect(args: &InspectArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let registry = ImageRegistry::load(&store)?;

    let local_name = resolve_local_name(&store, &args.name)?;
    let entry = registry
        .get(&local_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "image '{}' not found locally\n\
                 Hint: run 'ember image list' to see available images",
                args.name,
            )
        })?;

    match args.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(entry)?);
        }
        OutputFormat::Table => {
            println!("Reference:   {}", entry.reference);
            println!("Local name:  {}", entry.local_name);
            println!("ZFS zvol:    {}", entry.zvol);
            println!("Size:        {} MiB", entry.size_mib);
            println!("Pulled:      {}", entry.pulled_at);
        }
    }

    Ok(())
}

/// Resolve a user-provided image name to its registry local_name.
///
/// Tries parsing as an OCI reference first (so `alpine` resolves to
/// `library-alpine-latest`).  Falls back to a direct local_name lookup
/// so that locally built images (e.g. `ubuntu-vm`) work too.
fn resolve_local_name(store: &StateStore, name: &str) -> anyhow::Result<String> {
    let registry = ImageRegistry::load(store)?;

    // Try OCI reference parse → local_name.
    let reference = ImageReference::parse(name)?;
    let oci_local = reference.local_name();
    if registry.exists(&oci_local) {
        return Ok(oci_local);
    }

    // Fall back to direct local_name (for locally built images).
    if registry.exists(name) {
        return Ok(name.to_string());
    }

    anyhow::bail!(
        "image '{}' not found locally\n\
         Hint: run 'ember image list' to see available images, \
         or 'ember image pull {}' to pull it",
        name,
        name,
    )
}
