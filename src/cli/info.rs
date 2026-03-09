use std::path::Path;

use crate::cli::init::GlobalConfig;
use crate::image::registry::ImageRegistry;
use crate::state::store::StateStore;
use crate::state::vm;

pub fn run(state_dir: &Path) -> anyhow::Result<()> {
    let Some(store) = StateStore::try_open(state_dir) else {
        anyhow::bail!(
            "ember is not initialized (no state directory at {})\n\
             Run: ember init --pool <pool> --device <device>",
            state_dir.display()
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
    println!("ZFS pool:    {}", config.pool);
    println!("Dataset:     {}/{}", config.pool, config.dataset);
    println!(
        "Kernel:      {}",
        config
            .kernel_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(downloaded on first vm create)".to_string())
    );
    println!(
        "WAN iface:   {}",
        config.wan_iface.as_deref().unwrap_or("(not set)")
    );
    println!("Images:      {}", images.len());
    println!("VMs:         {} ({} running)", vms.len(), running);

    Ok(())
}
