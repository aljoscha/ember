use std::path::Path;

use crate::config::GlobalConfig;
use crate::image::registry::ImageRegistry;
use crate::state::store::StateStore;
use crate::state::vm;

pub fn run(state_dir: &Path) -> anyhow::Result<()> {
    let Some(store) = StateStore::try_open(state_dir) else {
        #[cfg(target_os = "linux")]
        anyhow::bail!(
            "ember is not initialized (no state directory at {})\n\
             Run: ember init --pool <pool> --device <device>",
            state_dir.display()
        );
        #[cfg(target_os = "macos")]
        anyhow::bail!(
            "ember is not initialized (no state directory at {})\n\
             Run: ember init",
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

    // ZFS pool/dataset are Linux-only; macOS uses APFS clones.
    #[cfg(target_os = "linux")]
    {
        println!("ZFS pool:    {}", config.pool);
        println!("Dataset:     {}/{}", config.pool, config.dataset);
    }

    println!(
        "Kernel:      {}",
        config
            .kernel_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(downloaded on first vm create)".to_string())
    );

    // WAN interface is Linux-only (used for iptables NAT rules).
    #[cfg(target_os = "linux")]
    println!(
        "WAN iface:   {}",
        config.wan_iface.as_deref().unwrap_or("(not set)")
    );

    println!("Images:      {}", images.len());
    println!("VMs:         {} ({} running)", vms.len(), running);

    Ok(())
}
