//! CPU containment for an installation's hypervisor processes.
//!
//! Every Firecracker process of one ember installation is placed in a
//! single cgroup v2 group named by [`name`], created at the root of the
//! hierarchy. The group carries a `cpu.weight` below the 100 that
//! `user.slice` and `system.slice` hold by default, so when the VMs go
//! full tilt they yield to the rest of the host instead of starving it.
//!
//! The group sits at the root on purpose. `cpu.weight` is only
//! meaningful between siblings, so nesting it under `system.slice`
//! would make it compete for that slice's share rather than for the
//! machine. As a root-level sibling it competes with the user's session
//! directly.
//!
//! Weight is a proportional share, not a cap. An idle host still lets
//! the VMs use every core. The limit bites only under contention, and
//! it bites the group as a whole: a dozen vcpu threads split one
//! sibling's share between them instead of each queueing against the
//! user's shell on equal terms. That grouping is the entire point.
//! Per-process priority does not survive overprovisioning, because the
//! more vCPUs we hand out, the more runnable threads compete.
//!
//! Placement is best-effort at the call site. A host without cgroup v2,
//! or without a usable `cpu` controller, still boots VMs. It just boots
//! them unconstrained.

use std::fs;
use std::path::{Path, PathBuf};

use ember_core::config::GlobalConfig;
use ember_core::error::{Error, Result};

/// cgroup v2 mount point, fixed by convention on any host running a
/// unified hierarchy.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// `cpu.weight` used when [`GlobalConfig::cpu_weight`] pins nothing.
///
/// Half the 100 that `user.slice` and `system.slice` default to. With
/// the user's session as the only other contender, saturated VMs settle
/// at roughly a third of the host, and they still take all of it when
/// nothing else wants CPU.
pub const DEFAULT_CPU_WEIGHT: u32 = 50;

/// cgroup name for an installation.
///
/// `instance_id` is `Some(ns)` for a per-installation group
/// (`ember-{ns}`) and `None` for legacy configs that predate
/// per-installation isolation. Two legacy installs on one host then
/// share the plain `ember` group, and so share one CPU budget. That is
/// the same conflation their common TAP prefix and pool name already
/// have.
pub fn name(instance_id: Option<&str>) -> String {
    match instance_id {
        None => "ember".to_string(),
        Some(id) => format!("ember-{id}"),
    }
}

/// The `cpu.weight` this installation applies to its VM group.
pub fn weight(config: &GlobalConfig) -> u32 {
    config.cpu_weight.unwrap_or(DEFAULT_CPU_WEIGHT)
}

/// Absolute path of the installation's cgroup directory.
fn path(instance_id: Option<&str>) -> PathBuf {
    Path::new(CGROUP_ROOT).join(name(instance_id))
}

/// Place a hypervisor process in the installation's CPU group, creating
/// the group with the configured weight if it does not exist yet.
///
/// Call this before the VM boots. cgroup v2 migrates every thread of a
/// process together, so this single write also covers all vcpu threads
/// the VM goes on to create.
///
/// The weight is rewritten on every call, so changing `cpu_weight` in
/// `config.json` takes effect the next time a VM starts. No reinit
/// needed.
pub fn place(pid: u32, config: &GlobalConfig) -> Result<()> {
    ensure_cpu_controller()?;

    let dir = path(config.instance_namespace());
    match fs::create_dir(&dir) {
        Ok(()) => {}
        // Another `ember vm start` got here first, which is fine. We
        // check for the error rather than for `exists()` so concurrent
        // starts can't both see "missing" and race on the create.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(Error::Io {
                path: dir,
                source: e,
            })
        }
    }

    write(&dir.join("cpu.weight"), &weight(config).to_string())?;
    write(&dir.join("cgroup.procs"), &pid.to_string())
}

/// Remove the installation's CPU group.
///
/// The kernel refuses to remove a populated group, so this only
/// succeeds once every hypervisor in it has exited. `ember deinit`,
/// the only caller, already refuses to run while VMs are registered.
pub fn remove(config: &GlobalConfig) -> Result<()> {
    let dir = path(config.instance_namespace());
    match fs::remove_dir(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io {
            path: dir,
            source: e,
        }),
    }
}

/// Verify the host runs a unified hierarchy with the `cpu` controller
/// available to root-level groups, turning it on if it is not.
///
/// systemd enables `cpu` in the root's `subtree_control` at boot, so
/// this is a no-op on most hosts. Where it isn't, a group we created
/// would come up with no `cpu.weight` file at all and the limit would
/// silently do nothing, so we enable the controller rather than report
/// a weight we never applied. The kernel rejects the write instead of
/// half-applying it, so a failure here is safe to propagate.
fn ensure_cpu_controller() -> Result<()> {
    // A host without a unified hierarchy has no such file, so the read
    // doubles as the cgroup v2 check. The path in the error names the
    // problem well enough that a separate probe would only add noise.
    let control = Path::new(CGROUP_ROOT).join("cgroup.subtree_control");
    let enabled = fs::read_to_string(&control).map_err(|e| Error::Io {
        path: control.clone(),
        source: e,
    })?;

    if has_controller(&enabled, "cpu") {
        return Ok(());
    }
    write(&control, "+cpu")
}

/// Whether `list`, the contents of a `cgroup.subtree_control` or
/// `cgroup.controllers` file, names `controller`.
///
/// Compares whole tokens on purpose. A substring test would find `cpu`
/// inside `cpuset` and conclude the CPU controller is on when it isn't.
fn has_controller(list: &str, controller: &str) -> bool {
    list.split_whitespace().any(|c| c == controller)
}

fn write(path: &Path, value: &str) -> Result<()> {
    fs::write(path, value).map_err(|e| Error::Io {
        path: path.to_path_buf(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_scoped_to_the_instance() {
        assert_eq!(name(Some("a3f4")), "ember-a3f4");
    }

    #[test]
    fn legacy_configs_share_one_group() {
        assert_eq!(name(None), "ember");
    }

    #[test]
    fn controller_match_is_token_wise() {
        assert!(has_controller("cpuset cpu io memory", "cpu"));
        assert!(has_controller("cpu", "cpu"));
        assert!(!has_controller("cpuset io memory", "cpu"));
        assert!(!has_controller("", "cpu"));
    }
}
