use std::path::Path;

use crate::backend::{CurrentPlatform, Platform};
use crate::cli::fmt::{format_bytes_binary, format_bytes_opt, format_percent, format_ratio};
use ember_core::config::GlobalConfig;
use ember_core::image::registry::ImageRegistry;
use ember_core::state::store::StateStore;
use ember_core::state::vm;

pub fn run(state_dir: &Path) -> anyhow::Result<()> {
    let Some(store) = StateStore::try_open(state_dir) else {
        anyhow::bail!(
            "ember is not initialized (no state directory at {})\n{}",
            state_dir.display(),
            CurrentPlatform::init_hint()
        );
    };

    let config: GlobalConfig = store.read(&store.config_path())?;

    let images = ImageRegistry::load(&store)?;
    let vms = vm::list(&store)?;
    let running = vms
        .iter()
        .filter(|v| v.status == vm::VmStatus::Running)
        .count();

    println!("State dir:   {}", state_dir.display());

    for (label, value) in CurrentPlatform::info_extra(&config) {
        println!("{:<13}{}", label, value);
    }

    println!(
        "Kernel:      {}",
        config
            .kernel_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(downloaded on first vm create)".to_string())
    );

    println!("Images:      {}", images.len());
    println!("VMs:         {} ({} running)", vms.len(), running);

    // Best-effort: `ember info` is the command you reach for when
    // something is wrong, so it must not fail just because the pool
    // cannot be measured. `ember storage usage` is the strict version.
    if let Some(usage) = crate::backend::try_usage(&config, &vms, &images.images) {
        let pool = &usage.pool;
        println!(
            "Capacity:    {} of {} used ({}), {} free",
            format_bytes_binary(pool.allocated),
            format_bytes_binary(pool.capacity),
            format_percent(pool.allocated, pool.capacity),
            format_bytes_binary(pool.free()),
        );
        if let (Some(logical), Some(ratio)) = (pool.logical, pool.compression_ratio()) {
            println!(
                "Compression: {} logical ({})",
                format_bytes_binary(logical),
                format_ratio(Some(ratio)),
            );
        }
        // Only when it differs from the physical capacity printed
        // above, which is exactly when the pool can promise more than
        // it holds.
        if let Some(addressable) = pool.addressable.filter(|a| *a != pool.capacity) {
            println!(
                "Addressable: {} exposed, {} left to hand out",
                format_bytes_binary(addressable),
                format_bytes_opt(pool.addressable_free()),
            );
        }
    }

    Ok(())
}
