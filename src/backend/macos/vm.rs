//! macOS VM backend: Apple Virtualization Framework via ember-vz helper.
//!
//! Spawns and signals the `ember-vz` Swift helper process to manage VMs.
//! The helper communicates back via a ready-fd (MAC address on boot)
//! and responds to Unix signals (SIGTERM, SIGUSR1, SIGUSR2).

use crate::backend::{StartedVm, VmBackend};
use crate::cli::init::GlobalConfig;
use crate::error::Result;
use crate::state::vm::VmMetadata;

/// macOS VM backend using Apple Virtualization Framework (via ember-vz).
pub struct MacosVm;

impl VmBackend for MacosVm {
    fn start(_vm: &VmMetadata, _config: &GlobalConfig) -> Result<StartedVm> {
        todo!("macOS: start VM via ember-vz")
    }

    fn stop(_vm: &VmMetadata) -> Result<()> {
        todo!("macOS: stop VM (SIGTERM to ember-vz)")
    }

    fn force_stop(_vm: &VmMetadata) -> Result<()> {
        todo!("macOS: force stop VM (SIGKILL to ember-vz)")
    }

    fn pause(_vm: &VmMetadata) -> Result<()> {
        todo!("macOS: pause VM (SIGUSR1 to ember-vz)")
    }

    fn resume(_vm: &VmMetadata) -> Result<()> {
        todo!("macOS: resume VM (SIGUSR2 to ember-vz)")
    }

    fn is_running(pid: u32) -> bool {
        // kill(pid, 0) works the same on macOS as Linux.
        unsafe { nix::libc::kill(pid as i32, 0) == 0 }
    }
}
