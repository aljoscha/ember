//! `ember storage` subcommands: pool-level administration.

use std::collections::BTreeMap;
use std::path::Path;

use clap::{Args, Subcommand};

use crate::backend::{create_storage, StorageUsage, VolumeUsage};
use crate::cli::fmt::{
    format_bytes_binary, format_bytes_opt, format_percent, format_ratio, print_table, Align,
};
use crate::cli::vm::OutputFormat;
use ember_core::config::size::ByteSize;
use ember_core::config::GlobalConfig;
use ember_core::image::registry::ImageRegistry;
use ember_core::state::store::StateStore;
use ember_core::state::vm;

#[derive(Subcommand)]
pub enum StorageCommand {
    /// Grow the underlying pool capacity (dm-thin only).
    Grow(GrowArgs),

    /// Report actual disk usage per VM and image.
    Usage(UsageArgs),
}

#[derive(Args)]
pub struct GrowArgs {
    /// New total size for the data device, e.g. `100G`. Must be larger
    /// than the current size.
    #[arg(long)]
    pub size: ByteSize,
}

#[derive(Args)]
pub struct UsageArgs {
    /// Output format
    #[arg(long, value_enum, default_value_t = OutputFormat::Table)]
    pub format: OutputFormat,
}

pub fn run(cmd: &StorageCommand, state_dir: &Path) -> anyhow::Result<()> {
    match cmd {
        StorageCommand::Grow(args) => grow(args, state_dir),
        StorageCommand::Usage(args) => usage(args, state_dir),
    }
}

fn grow(args: &GrowArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = create_storage(&config);
    storage.grow(args.size)?;
    Ok(())
}

/// Unlike the best-effort usage shown by `vm list` and `info`, this
/// command reports a backend failure as a failure. Being unable to
/// measure is the one thing it exists to do.
fn usage(args: &UsageArgs, state_dir: &Path) -> anyhow::Result<()> {
    let store = StateStore::new(state_dir.to_path_buf());
    let config: GlobalConfig = store.read(&store.config_path())?;
    let storage = create_storage(&config);

    let vms = vm::list(&store)?;
    let images = ImageRegistry::load(&store)?;
    let usage = storage.usage(&vms, &images.images)?;

    match args.format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&usage)?),
        OutputFormat::Table => print_usage(&usage),
    }
    Ok(())
}

fn print_usage(usage: &StorageUsage) {
    let pool = &usage.pool;
    println!();
    println!(
        "Pool          {} capacity, {} used ({}), {} free",
        format_bytes_binary(pool.capacity),
        format_bytes_binary(pool.allocated),
        format_percent(pool.allocated, pool.capacity),
        format_bytes_binary(pool.free()),
    );
    // Against `occupied`, not `allocated`: the ratio divides by it, and
    // printing the larger figure here would make the line contradict
    // its own arithmetic.
    if let (Some(logical), Some(ratio)) = (pool.logical, pool.compression_ratio()) {
        println!(
            "Compression   {} logical -> {} on disk ({})",
            format_bytes_binary(logical),
            format_bytes_binary(pool.occupied()),
            format_ratio(Some(ratio)),
        );
    }
    if pool.reserved > 0 {
        println!(
            "Reserved      {} charged to the pool but holding no data",
            format_bytes_binary(pool.reserved),
        );
    }
    if let Some(meta) = pool.metadata {
        println!(
            "Metadata      {} of {} used ({})",
            format_bytes_binary(meta.used),
            format_bytes_binary(meta.capacity),
            format_percent(meta.used, meta.capacity),
        );
    }

    print_section("VMS", &usage.vms);
    print_section("IMAGES", &usage.images);
    println!();
}

fn print_section(heading: &str, volumes: &BTreeMap<String, VolumeUsage>) {
    if volumes.is_empty() {
        return;
    }
    println!();
    println!("{heading}");
    let rows: Vec<Vec<String>> = volumes
        .iter()
        .map(|(name, u)| {
            vec![
                name.clone(),
                format_bytes_binary(u.provisioned),
                format_bytes_opt(u.referenced),
                format_bytes_binary(u.exclusive),
                format_bytes_opt(u.shared()),
                format_ratio(u.compression_ratio()),
            ]
        })
        .collect();
    print_table(
        &[
            "NAME",
            "PROVISIONED",
            "REFERENCED",
            "EXCLUSIVE",
            "SHARED",
            "COMPRESSION",
        ],
        &[
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ],
        &rows,
    );
}
