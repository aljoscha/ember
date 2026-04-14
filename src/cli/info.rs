use std::path::Path;

use crate::backend::{CurrentPlatform, Platform};
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

    Ok(())
}
