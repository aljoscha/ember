use std::path::Path;

use clap::{Args, Subcommand};

use super::init::GlobalConfig;
use super::vm::OutputFormat;
use crate::image;
use crate::image::pull::ImageReference;
use crate::image::registry::{ImageRegistry, new_entry};
use crate::state::store::StateStore;
use crate::zfs;

#[derive(Subcommand)]
pub enum ImageCommand {
    /// Pull an OCI image from a registry
    Pull(PullArgs),

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
pub struct ListArgs {
    /// Output format
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// Image name
    pub name: String,
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
        ImageCommand::List(args) => list(args, state_dir),
        ImageCommand::Delete(args) => delete(args, state_dir),
        ImageCommand::Inspect(args) => inspect(args, state_dir),
    }
}

/// Pull an OCI image from a registry and write it to a ZFS zvol.
///
/// Full pipeline: skopeo pull → inject SSH keys + resolv.conf → ext4 image
/// → zvol create → dd to zvol → @base snapshot → register in local registry.
fn pull(args: &PullArgs, state_dir: &Path) -> anyhow::Result<()> {
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

    // Step 3: Create ext4 filesystem image from rootfs.
    let size_mib = image::ext4::estimate_size_mib(&rootfs_dir)?;
    let ext4_path = work_dir.path().join("rootfs.ext4");
    println!("  Creating ext4 image ({size_mib} MiB)...");
    image::ext4::create(&rootfs_dir, &ext4_path, size_mib)?;

    // Step 4: Create ZFS zvol and write ext4 image to it.
    println!("  Creating zvol {zvol}...");
    zfs::volume::create(&zvol, size_mib)?;

    println!("  Writing image to zvol and creating @base snapshot...");
    if let Err(e) = image::zvol::write_to_zvol(&ext4_path, &zvol) {
        // Clean up the zvol on failure.
        let _ = zfs::volume::destroy(&zvol, true);
        return Err(e.into());
    }

    // Step 5: Register in local image registry.
    let entry = new_entry(&reference, &zvol, size_mib);
    let mut registry = ImageRegistry::load(&store)?;
    registry.add(entry);
    registry.save(&store)?;

    println!("Image '{reference}' pulled successfully as '{local_name}'.");
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
fn delete(args: &DeleteArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());

    // Look up the image by parsing the user-provided name as a reference.
    // This allows both "alpine" and "docker.io/library/alpine:latest".
    let reference = ImageReference::parse(&args.name)?;
    let local_name = reference.local_name();

    let entry = image::registry::remove_image(&store, &local_name)?;

    // Destroy the ZFS zvol (and its @base snapshot) recursively.
    if zfs::volume::exists(&entry.zvol)? {
        println!("Destroying zvol {}...", entry.zvol);
        zfs::volume::destroy(&entry.zvol, true)?;
    }

    println!("Image '{}' deleted.", entry.reference);
    Ok(())
}

/// Show detailed information about a local image.
fn inspect(args: &InspectArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let registry = ImageRegistry::load(&store)?;

    let reference = ImageReference::parse(&args.name)?;
    let entry = registry
        .get(&reference.local_name())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "image '{}' not found locally — pull it first with: ember image pull {}",
                args.name,
                args.name
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
