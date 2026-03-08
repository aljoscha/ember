use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use super::init::GlobalConfig;
use super::vm::OutputFormat;
use crate::backend::{Storage, StorageBackend};
use crate::image;
use crate::image::pull::ImageReference;
use crate::image::registry::{new_build_entry, new_entry, ImageRegistry};
use crate::state::store::StateStore;
use crate::state::vm::{self, VmMetadata};

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
    /// Image name (e.g. ubuntu-dev, my-image:v1)
    pub name: String,

    /// Path to Dockerfile (default: built-in Ubuntu VM image)
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

/// Pull an OCI image from a registry and import it into storage.
///
/// Full pipeline: skopeo pull → inject SSH keys + resolv.conf → ext4 image
/// → storage backend import → register in local registry.
///
/// Uses a [`Rollback`] guard to ensure storage is cleaned up if any step
/// after creation fails (e.g., saving the registry).
fn pull(args: &PullArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);

    // Parse and validate the image reference.
    let reference = ImageReference::parse(&args.reference)?;
    let local_name = reference.local_name();

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

    // Step 2: Inject SSH authorized_keys, resolv.conf, and inittab into rootfs.
    inject_image_config(&rootfs_dir, true)?;

    // Steps 3-4: Create ext4 image → import into storage backend.
    let (size_mib, disk_path, rollback) =
        create_image_from_rootfs(&rootfs_dir, work_dir.path(), &local_name, &storage)?;

    // Step 5: Register in local image registry.
    let zvol = disk_path.to_string_lossy().to_string();
    let entry = new_entry(&reference, &zvol, size_mib);
    let mut registry = ImageRegistry::load(&store)?;
    registry.add(entry);
    registry.save(&store)?;

    rollback.commit();

    println!("Image '{reference}' pulled successfully as '{local_name}'.");
    Ok(())
}

/// Build a VM image from a Dockerfile and import it into storage.
///
/// Full pipeline: docker build → export rootfs → inject SSH keys + resolv.conf
/// → ext4 image → storage backend import → register.
fn build(args: &BuildArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);

    // Sanitize the name for storage use.
    let local_name = image::build::sanitize_name(&args.name)?;

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
            std::fs::write(&default_path, image::build::DEFAULT_DOCKERFILE).map_err(|e| {
                crate::error::Error::Io {
                    path: default_path.clone(),
                    source: e,
                }
            })?;
            default_path
        }
    };

    // Step 1: Build container image and export rootfs.
    println!("  Building container image...");
    let rootfs_dir = image::build::build(&dockerfile, work_dir.path(), &local_name)?;

    // Step 2: Inject SSH authorized_keys and resolv.conf into rootfs.
    // Skip inittab — systemd-based images handle init and CtrlAltDel natively.
    inject_image_config(&rootfs_dir, false)?;

    // Steps 3-4: Create ext4 image → import into storage backend.
    let (size_mib, disk_path, rollback) =
        create_image_from_rootfs(&rootfs_dir, work_dir.path(), &local_name, &storage)?;

    // Step 5: Register in local image registry.
    let zvol = disk_path.to_string_lossy().to_string();
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

    // Look up the image entry (don't remove from registry yet — the zvol
    // destroy might fail if there are dependent clones).
    let registry = ImageRegistry::load(&store)?;
    let local_name = resolve_local_name(&registry, &args.name)?;
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

    // Destroy the image's storage (zvol on Linux, .img file on macOS).
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = Storage::new(&config);
    println!("Destroying storage for image '{}'...", local_name);
    storage.destroy_image_storage(&local_name)?;

    // Remove from registry last, after the zvol is gone.
    image::registry::remove_image(&store, &local_name)?;

    println!("Image '{}' deleted.", entry.reference);
    Ok(())
}

/// Force-delete a single VM: delegates to the shared `vm::force_delete_vm`.
fn force_delete_vm(store: &StateStore, metadata: &VmMetadata) -> anyhow::Result<()> {
    println!("Deleting dependent VM '{}'...", metadata.name);
    super::vm::force_delete_vm(store, metadata)
}

/// Show detailed information about a local image.
fn inspect(args: &InspectArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let registry = ImageRegistry::load(&store)?;

    let local_name = resolve_local_name(&registry, &args.name)?;
    let entry = registry.get(&local_name).ok_or_else(|| {
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

/// Inject SSH public key and resolv.conf into a rootfs directory.
///
/// If `inject_inittab` is true, also injects an inittab for lightweight
/// init-based images (e.g., Alpine from OCI pull). Systemd-based images
/// (from `image build`) skip inittab injection.
fn inject_image_config(rootfs_dir: &Path, inject_inittab: bool) -> anyhow::Result<()> {
    if let Some(pubkey_path) = image::inject::default_ssh_pubkey_path() {
        if pubkey_path.exists() {
            println!(
                "  Injecting SSH public key from {}...",
                pubkey_path.display()
            );
            image::inject::inject_ssh_authorized_keys(rootfs_dir, &pubkey_path)?;
        } else {
            println!(
                "  Warning: SSH public key not found at {}, skipping injection.",
                pubkey_path.display()
            );
        }
    }
    image::inject::inject_resolv_conf(rootfs_dir)?;
    if inject_inittab {
        image::inject::inject_inittab(rootfs_dir)?;
    }
    Ok(())
}

/// Create an ext4 image from a rootfs directory and import it into storage.
///
/// Returns `(size_mib, disk_path, rollback)` — the caller must register
/// the image in the registry and then call `rollback.commit()` to finalize.
fn create_image_from_rootfs(
    rootfs_dir: &Path,
    work_dir: &Path,
    name: &str,
    storage: &Storage,
) -> anyhow::Result<(u64, PathBuf, crate::cleanup::Rollback)> {
    let size_mib = image::ext4::estimate_size_mib(rootfs_dir)?;
    let ext4_path = work_dir.join("rootfs.ext4");
    println!("  Creating ext4 image ({size_mib} MiB)...");
    image::ext4::create(rootfs_dir, &ext4_path, size_mib)?;

    println!("  Importing image into storage...");
    let disk_path = storage.create_image_volume(name, &ext4_path, size_mib)?;

    let mut rollback = crate::cleanup::Rollback::new();
    {
        let storage = storage.clone();
        let n = name.to_string();
        rollback.push("image storage", move || {
            let _ = storage.destroy_image_storage(&n);
        });
    }

    Ok((size_mib, disk_path, rollback))
}

/// Resolve a user-provided image name to its registry local_name.
///
/// Tries parsing as an OCI reference first (so `alpine` resolves to
/// `library-alpine-latest`).  Falls back to a direct local_name lookup
/// so that locally built images (e.g. `ubuntu-vm`) work too.
fn resolve_local_name(registry: &ImageRegistry, name: &str) -> anyhow::Result<String> {
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
