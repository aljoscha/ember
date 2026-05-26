use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use super::fmt::{format_bytes_binary, print_table, Align, MIB};
use super::vm::OutputFormat;
use crate::backend::{create_storage, CurrentPlatform, Platform, Storage, VolumeHandle};
use crate::image;
use ember_core::config::GlobalConfig;
use ember_core::image::pull::ImageReference;
use ember_core::image::registry::{new_build_entry, new_entry, ImageEntry, ImageRegistry};
use ember_core::state::store::StateStore;
use ember_core::state::vm::{self, VmMetadata};

#[derive(Subcommand)]
pub enum ImageCommand {
    /// Pull an OCI image from a registry
    Pull(PullArgs),

    /// Build a VM image from a Dockerfile
    Build(BuildArgs),

    /// List locally available images
    List(ListArgs),

    /// Delete a local image
    #[command(visible_alias = "delete")]
    Rm(RmArgs),

    /// Show detailed image information
    Inspect(InspectArgs),

    /// Rename a local image
    Rename(RenameArgs),
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
pub struct RmArgs {
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

#[derive(Args)]
pub struct RenameArgs {
    /// Current image name (reference or local name)
    pub name: String,

    /// New local name
    pub new_name: String,
}

pub fn run(cmd: &ImageCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        ImageCommand::Pull(args) => pull(args, state_dir),
        ImageCommand::Build(args) => build(args, state_dir),
        ImageCommand::List(args) => list(args, state_dir),
        ImageCommand::Rm(args) => rm(args, state_dir),
        ImageCommand::Inspect(args) => inspect(args, state_dir),
        ImageCommand::Rename(args) => rename(args, state_dir),
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
    let storage = create_storage(&config);

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
    let work_dir = tempfile::tempdir().map_err(|e| ember_core::error::Error::Io {
        path: std::env::temp_dir(),
        source: e,
    })?;

    // Step 1: Pull OCI image and unpack layers.
    println!("  Downloading and unpacking layers...");
    let rootfs_dir = image::pull::pull(
        &reference,
        work_dir.path(),
        &CurrentPlatform::image_tool_config(),
    )?;

    // Step 2: Inject SSH authorized_keys, resolv.conf, and inittab into rootfs.
    inject_image_config(&rootfs_dir, true)?;

    // Steps 3-4: Create ext4 image → import into storage backend.
    let (size_mib, handle, rollback) =
        create_image_from_rootfs(&rootfs_dir, work_dir.path(), &local_name, &storage)?;

    // Step 5: Register in local image registry.
    let disk = handle.disk_path.to_string_lossy().to_string();
    let entry = new_entry(&reference, &disk, size_mib, handle.thin_id);
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
    let storage = create_storage(&config);

    // Sanitize the name for storage use.
    let local_name = image::build::sanitize_name(&args.name)?;

    // Check if this image already exists.
    let registry = ImageRegistry::load(&store)?;
    if registry.exists(&local_name) {
        println!("Image '{}' already exists locally.", local_name);
        return Ok(());
    }

    println!("Building image '{}'...", args.name);

    let work_dir = tempfile::tempdir().map_err(|e| ember_core::error::Error::Io {
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
                ember_core::error::Error::Io {
                    path: default_path.clone(),
                    source: e,
                }
            })?;
            default_path
        }
    };

    // Step 1: Build container image and export rootfs.
    println!("  Building container image...");
    let rootfs_dir = image::build::build(
        &dockerfile,
        work_dir.path(),
        &local_name,
        &CurrentPlatform::image_tool_config(),
    )?;

    // Step 2: Inject SSH authorized_keys and resolv.conf into rootfs.
    // Skip inittab — systemd-based images handle init and CtrlAltDel natively.
    inject_image_config(&rootfs_dir, false)?;

    // Steps 3-4: Create ext4 image → import into storage backend.
    let (size_mib, handle, rollback) =
        create_image_from_rootfs(&rootfs_dir, work_dir.path(), &local_name, &storage)?;

    // Step 5: Register in local image registry.
    let disk = handle.disk_path.to_string_lossy().to_string();
    let entry = new_build_entry(&args.name, &local_name, &disk, size_mib, handle.thin_id);
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

            let rows: Vec<Vec<String>> = registry
                .images
                .iter()
                .map(|img| {
                    vec![
                        img.reference.clone(),
                        img.local_name.clone(),
                        format_bytes_binary(img.size_mib * MIB),
                        img.pulled_at.clone(),
                    ]
                })
                .collect();
            print_table(
                &["REFERENCE", "LOCAL NAME", "SIZE", "PULLED"],
                &[Align::Left, Align::Left, Align::Right, Align::Left],
                &rows,
            );
        }
    }

    Ok(())
}

/// Delete a local image: remove from registry and destroy backing storage.
///
/// If VMs were cloned from this image, they hold a ZFS dependency on the
/// image's `@base` snapshot. Without `--force`, the command lists the
/// dependent VMs and exits. With `--force`, it deletes those VMs first,
/// then destroys the image.
fn rm(args: &RmArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    // Look up the image entry (don't remove from registry yet — the storage
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
    let storage = create_storage(&config);
    println!("Destroying storage for image '{}'...", local_name);
    storage.destroy_image_storage(&entry, args.force)?;

    // Remove from registry last, after the storage is gone.
    image::registry::remove_image(&store, &local_name)?;

    println!("Image '{}' deleted.", entry.reference);
    Ok(())
}

/// Rename a local image.
///
/// Renames the underlying storage and rewrites the registry entry's
/// `local_name` and `disk_path`. For locally built images (`reference`
/// starts with `local:`) the reference is rewritten too, and any VMs
/// recorded with the old `local:` reference are updated so
/// `image delete` continues to see them as dependents.
fn rename(args: &RenameArgs, state_dir: &Path) -> anyhow::Result<()> {
    if args.new_name.is_empty() {
        anyhow::bail!("new name must not be empty");
    }

    let store = StateStore::new(state_dir.to_path_buf());

    // Normalise the new local name through the build sanitiser so it's
    // valid for ZFS datasets / dm device names / APFS files.
    let new_local_name = image::build::sanitize_name(&args.new_name)?;

    let mut registry = ImageRegistry::load(&store)?;
    let old_local_name = resolve_local_name(&registry, &args.name)?;

    if old_local_name == new_local_name {
        anyhow::bail!("new name is the same as the current name");
    }
    if registry.exists(&new_local_name) {
        anyhow::bail!("image '{new_local_name}' already exists locally");
    }

    let mut entry = registry
        .get(&old_local_name)
        .ok_or_else(|| anyhow::anyhow!("image '{old_local_name}' not found locally"))?
        .clone();

    let old_reference = entry.reference.clone();

    // Rename the underlying storage first; rolling back the registry
    // is cheap, rolling back the disk after a partial registry write
    // is not.
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = create_storage(&config);
    let new_handle = storage.rename_image_storage(&entry, &new_local_name)?;

    // Update the registry entry. For built images, the user-facing
    // reference is `local:<name>` — rewrite it so list/inspect show
    // the new identity.
    entry.local_name = new_local_name.clone();
    entry.disk_path = new_handle.disk_path.to_string_lossy().into_owned();
    entry.thin_id = new_handle.thin_id;
    let new_reference = if old_reference.starts_with("local:") {
        format!("local:{}", args.new_name)
    } else {
        old_reference.clone()
    };
    entry.reference = new_reference.clone();

    registry.remove(&old_local_name);
    registry.add(entry);
    registry.save(&store)?;

    // If the user-facing reference changed (built-image case), bring
    // dependent VM records along so `image delete` and similar
    // reference-matched lookups keep working.
    if new_reference != old_reference {
        for mut v in vm::list(&store)? {
            if v.image == old_reference {
                v.image = new_reference.clone();
                vm::save(&store, &v)?;
            }
        }
    }

    println!("Image '{old_local_name}' renamed to '{new_local_name}'.");
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
            for (label, value) in CurrentPlatform::inspect_image_extra(entry) {
                println!("{:<13}{}", label, value);
            }
            println!("Size:        {}", format_bytes_binary(entry.size_mib * MIB));
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
    image::inject::inject_resolv_conf(rootfs_dir, &CurrentPlatform::resolv_conf_mode())?;
    if inject_inittab {
        image::inject::inject_inittab(rootfs_dir, CurrentPlatform::console_device())?;
    }
    Ok(())
}

/// Create an ext4 image from a rootfs directory and import it into storage.
///
/// Returns `(size_mib, handle, rollback)` — the caller pulls
/// `handle.disk_path` and `handle.thin_id` to build an [`ImageEntry`]
/// for the registry, then calls `rollback.commit()` to finalize.
fn create_image_from_rootfs(
    rootfs_dir: &Path,
    work_dir: &Path,
    name: &str,
    storage: &Storage,
) -> anyhow::Result<(u64, VolumeHandle, ember_core::cleanup::Rollback)> {
    let size_mib = CurrentPlatform::estimate_ext4_size_mib(rootfs_dir)?;
    let ext4_path = work_dir.join("rootfs.ext4");
    println!(
        "  Creating ext4 image ({})...",
        format_bytes_binary(size_mib * MIB)
    );
    CurrentPlatform::create_ext4_image(rootfs_dir, &ext4_path, size_mib)?;

    // Use the actual file size after shrink_to_fit, not the pre-shrink estimate.
    let size_mib = std::fs::metadata(&ext4_path)
        .map(|m| m.len() / MIB)
        .unwrap_or(size_mib);

    println!("  Importing image into storage...");
    let handle = storage.create_image_volume(name, &ext4_path, size_mib)?;

    let mut rollback = ember_core::cleanup::Rollback::new();
    {
        let storage = storage.clone();
        let stub = stub_image_entry(name, &handle);
        rollback.push("image storage", move || {
            let _ = storage.destroy_image_storage(&stub, false);
        });
    }

    Ok((size_mib, handle, rollback))
}

/// Build a minimal [`ImageEntry`] for use in cleanup paths where the
/// real entry hasn't been (or no longer is) registered. The ZFS, btrfs,
/// and dm-thin backends only inspect `local_name` and `thin_id`, so the
/// remaining fields can be placeholders.
fn stub_image_entry(local_name: &str, handle: &VolumeHandle) -> ImageEntry {
    ImageEntry {
        reference: String::new(),
        local_name: local_name.to_string(),
        disk_path: handle.disk_path.to_string_lossy().into_owned(),
        size_mib: 0,
        pulled_at: String::new(),
        thin_id: handle.thin_id,
    }
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
