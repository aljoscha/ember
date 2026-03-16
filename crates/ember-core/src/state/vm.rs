//! VM metadata types and state tracking.
//!
//! Each VM's metadata is stored as a separate JSON file at
//! `<state-dir>/vms/<name>/vm.json`. This module defines the
//! serializable types and convenience functions for loading,
//! saving, listing, and deleting VM metadata via [`StateStore`].

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::state::store::StateStore;

/// Lifecycle state of a VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmStatus {
    /// VM created but never started.
    Created,
    /// Firecracker process is running.
    Running,
    /// Firecracker process has exited (gracefully or killed).
    Stopped,
    /// Firecracker VM is paused (via API).
    Paused,
}

impl fmt::Display for VmStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmStatus::Created => write!(f, "created"),
            VmStatus::Running => write!(f, "running"),
            VmStatus::Stopped => write!(f, "stopped"),
            VmStatus::Paused => write!(f, "paused"),
        }
    }
}

/// Persisted network configuration for a running VM.
///
/// Tracks the allocated IP addresses and TAP device so they can
/// be cleaned up on stop/delete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkInfo {
    /// TAP device name on the host (e.g., "em-abc123").
    pub tap_device: String,
    /// Host-side IP of the point-to-point link (e.g., "10.100.0.1").
    pub host_ip: String,
    /// Guest-side IP (e.g., "10.100.0.2").
    pub guest_ip: String,
    /// Netmask for the /30 link (e.g., "255.255.255.252").
    pub netmask: String,
    /// Guest MAC address, if assigned.
    pub guest_mac: Option<String>,
    /// WAN interface used for iptables rules (e.g., "eth0", "wg0-mullvad").
    ///
    /// Stored so cleanup can remove the exact rules that were added,
    /// even if the default route changes between start and stop.
    #[serde(default)]
    pub wan_iface: Option<String>,
}

/// SSH connection configuration for a VM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshConfig {
    /// SSH user to connect as (default: "root").
    pub user: String,
    /// Path to the SSH private key.
    pub key: PathBuf,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self {
            user: "root".to_string(),
            key: PathBuf::from("/root/.ssh/id_ed25519"),
        }
    }
}

/// Full metadata for a VM, persisted as `vm.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmMetadata {
    /// User-chosen VM name (unique identifier).
    pub name: String,
    /// Unique ID for internal use (TAP device naming, etc.).
    pub id: Uuid,
    /// Current lifecycle state.
    pub status: VmStatus,
    /// Image reference this VM was created from (e.g., "docker.io/library/alpine:latest").
    pub image: String,
    /// Number of vCPUs.
    pub cpus: u32,
    /// Memory in MiB.
    pub memory_mib: u32,
    /// Disk size in GiB.
    pub disk_size_gib: u32,
    /// Path to the kernel image.
    pub kernel_path: PathBuf,
    /// Path to the root disk. On Linux, a ZFS zvol (e.g., "tank/ember/vms/myvm").
    /// On macOS, a raw disk image path (e.g., ".../vms/myvm/rootfs.img").
    #[serde(alias = "zvol_path")]
    pub disk_path: String,
    /// Custom kernel boot arguments. When set, replaces the default
    /// boot args; the `ip=` networking parameter is still appended.
    #[serde(default)]
    pub boot_args: Option<String>,
    /// Network subnet for IP allocation (e.g., "10.100.0.0/16").
    /// Defaults to [`network::ip::DEFAULT_SUBNET`] when not set.
    #[serde(default)]
    pub subnet: Option<String>,
    /// Network configuration, if networking is set up.
    pub network: Option<NetworkInfo>,
    /// PID of the running Firecracker process.
    pub pid: Option<u32>,
    /// Path to the Firecracker API socket.
    pub api_socket: PathBuf,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// SSH connection configuration.
    pub ssh: SshConfig,
    /// Name of the source VM if this VM was forked.
    ///
    /// Used on Linux/ZFS to clean up the fork snapshot on the source VM
    /// when this VM is deleted, and to enforce the "cannot delete a source
    /// VM while forks depend on it" constraint.
    ///
    /// On macOS/APFS, forks are fully independent (`cp -c`), so this field
    /// is purely informational — no cleanup or deletion constraints apply.
    #[serde(default, alias = "forked_from")]
    pub parent_vm: Option<String>,
}

impl VmMetadata {
    /// Create a minimal VmMetadata for use in backend teardown/cleanup.
    ///
    /// Only the `name` and `network` fields are meaningful; the rest are
    /// placeholder values. Used in rollback closures where we only need
    /// enough metadata to clean up resources.
    pub fn default_for_teardown() -> Self {
        Self {
            name: String::new(),
            id: Uuid::nil(),
            status: VmStatus::Created,
            image: String::new(),
            cpus: 0,
            memory_mib: 0,
            disk_size_gib: 0,
            kernel_path: PathBuf::new(),
            disk_path: String::new(),
            boot_args: None,
            subnet: None,
            network: None,
            pid: None,
            api_socket: PathBuf::new(),
            created_at: String::new(),
            ssh: SshConfig {
                user: String::new(),
                key: PathBuf::new(),
            },
            parent_vm: None,
        }
    }
}

/// Load a running VM's metadata and network info.
///
/// Returns an error if the VM is not found, not in `Running` state,
/// or has no network configured. Use this for commands that need SSH
/// access to a running VM.
pub fn load_running_with_network(
    store: &StateStore,
    name: &str,
) -> anyhow::Result<(VmMetadata, NetworkInfo)> {
    let metadata = load(store, name)?;
    if metadata.status != VmStatus::Running {
        anyhow::bail!(
            "vm '{}' is {} — start it first with: ember vm start {}",
            name,
            metadata.status,
            name
        );
    }
    let network = metadata.network.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "vm '{}' has no network configured — cannot connect via SSH",
            name
        )
    })?;
    Ok((metadata, network))
}

/// Load VM metadata and verify the VM is stopped (or never started).
///
/// Returns an error if the VM is currently running or paused.
/// `operation` is a human-readable verb like "resizing" or "restoring a snapshot"
/// used in the error message.
pub fn require_stopped(
    store: &StateStore,
    name: &str,
    operation: &str,
) -> anyhow::Result<VmMetadata> {
    let metadata = load(store, name)?;
    match metadata.status {
        VmStatus::Created | VmStatus::Stopped => {}
        VmStatus::Running => {
            anyhow::bail!("vm '{}' is running — stop it before {operation}", name);
        }
        VmStatus::Paused => {
            anyhow::bail!("vm '{}' is paused — stop it before {operation}", name);
        }
    }
    Ok(metadata)
}

/// Load VM metadata from the state store.
///
/// Returns [`Error::VmNotFound`] if no metadata file exists for `name`.
pub fn load(store: &StateStore, name: &str) -> Result<VmMetadata> {
    let path = store.vm_metadata_path(name);
    store
        .read_optional(&path)?
        .ok_or_else(|| Error::VmNotFound {
            name: name.to_string(),
        })
}

/// Save VM metadata to the state store.
///
/// Creates the per-VM directory and writes `vm.json`. Overwrites
/// any existing metadata for this VM.
pub fn save(store: &StateStore, vm: &VmMetadata) -> Result<()> {
    let path = store.vm_metadata_path(&vm.name);
    store.write(&path, vm)
}

/// List all VMs by reading metadata from each subdirectory under `vms/`.
///
/// Skips directories that don't contain a valid `vm.json` (e.g., partially
/// deleted VMs). Returns an empty vec if no VMs exist.
pub fn list(store: &StateStore) -> Result<Vec<VmMetadata>> {
    let vms_dir = store.root().join("vms");
    let entries = match std::fs::read_dir(&vms_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::Io {
                path: vms_dir,
                source: e,
            })
        }
    };

    let mut vms = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::Io {
            path: vms_dir.clone(),
            source: e,
        })?;

        // Only look at directories.
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // Try to read vm.json; skip if missing or unparseable.
        let meta_path = path.join("vm.json");
        if let Ok(Some(vm)) = store.read_optional::<VmMetadata>(&meta_path) {
            vms.push(vm);
        }
    }

    vms.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(vms)
}

/// Check whether a VM exists in the state store.
pub fn exists(store: &StateStore, name: &str) -> bool {
    store.vm_metadata_path(name).exists()
}

/// Delete a VM's state directory and all files within it.
///
/// Idempotent — succeeds even if the directory is already gone.
pub fn delete(store: &StateStore, name: &str) -> Result<()> {
    let dir = store.vm_dir(name);
    store.remove_dir(&dir)
}

/// Current UTC time as an ISO 8601 string (second precision).
///
/// Format: `YYYY-MM-DDTHH:MM:SSZ` (always UTC).
pub fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Break epoch seconds into date/time components.
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;

    // Civil date from day count (algorithm from Howard Hinnant).
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();
        (dir, store)
    }

    fn sample_vm(name: &str) -> VmMetadata {
        VmMetadata {
            name: name.to_string(),
            id: Uuid::nil(),
            status: VmStatus::Created,
            image: "docker.io/library/alpine:latest".to_string(),
            cpus: 2,
            memory_mib: 512,
            disk_size_gib: 4,
            kernel_path: PathBuf::from("/var/lib/ember/kernels/vmlinux"),
            disk_path: format!("tank/ember/vms/{name}"),
            boot_args: None,
            subnet: None,
            network: None,
            pid: None,
            api_socket: PathBuf::from(format!("/var/lib/ember/vms/{name}/firecracker.sock")),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            ssh: SshConfig::default(),
            parent_vm: None,
        }
    }

    #[test]
    fn status_display() {
        assert_eq!(VmStatus::Created.to_string(), "created");
        assert_eq!(VmStatus::Running.to_string(), "running");
        assert_eq!(VmStatus::Stopped.to_string(), "stopped");
        assert_eq!(VmStatus::Paused.to_string(), "paused");
    }

    #[test]
    fn status_serialization() {
        let json = serde_json::to_string(&VmStatus::Running).unwrap();
        assert_eq!(json, "\"running\"");

        let parsed: VmStatus = serde_json::from_str("\"paused\"").unwrap();
        assert_eq!(parsed, VmStatus::Paused);
    }

    #[test]
    fn ssh_config_defaults() {
        let ssh = SshConfig::default();
        assert_eq!(ssh.user, "root");
        assert_eq!(ssh.key, PathBuf::from("/root/.ssh/id_ed25519"));
    }

    #[test]
    fn save_and_load() {
        let (_dir, store) = test_store();
        let vm = sample_vm("testvm");

        save(&store, &vm).unwrap();
        let loaded = load(&store, "testvm").unwrap();
        assert_eq!(loaded, vm);
    }

    #[test]
    fn load_nonexistent_returns_not_found() {
        let (_dir, store) = test_store();
        let err = load(&store, "nope").unwrap_err();
        assert!(matches!(err, Error::VmNotFound { name } if name == "nope"));
    }

    #[test]
    fn save_overwrites_existing() {
        let (_dir, store) = test_store();
        let mut vm = sample_vm("testvm");
        save(&store, &vm).unwrap();

        vm.cpus = 4;
        vm.status = VmStatus::Running;
        vm.pid = Some(12345);
        save(&store, &vm).unwrap();

        let loaded = load(&store, "testvm").unwrap();
        assert_eq!(loaded.cpus, 4);
        assert_eq!(loaded.status, VmStatus::Running);
        assert_eq!(loaded.pid, Some(12345));
    }

    #[test]
    fn list_empty() {
        let (_dir, store) = test_store();
        let vms = list(&store).unwrap();
        assert!(vms.is_empty());
    }

    #[test]
    fn list_multiple_vms() {
        let (_dir, store) = test_store();
        save(&store, &sample_vm("beta")).unwrap();
        save(&store, &sample_vm("alpha")).unwrap();
        save(&store, &sample_vm("gamma")).unwrap();

        let vms = list(&store).unwrap();
        assert_eq!(vms.len(), 3);
        // Sorted by name.
        assert_eq!(vms[0].name, "alpha");
        assert_eq!(vms[1].name, "beta");
        assert_eq!(vms[2].name, "gamma");
    }

    #[test]
    fn exists_check() {
        let (_dir, store) = test_store();
        assert!(!exists(&store, "testvm"));

        save(&store, &sample_vm("testvm")).unwrap();
        assert!(exists(&store, "testvm"));
    }

    #[test]
    fn delete_removes_vm_dir() {
        let (_dir, store) = test_store();
        save(&store, &sample_vm("testvm")).unwrap();
        assert!(exists(&store, "testvm"));

        delete(&store, "testvm").unwrap();
        assert!(!exists(&store, "testvm"));
    }

    #[test]
    fn delete_idempotent() {
        let (_dir, store) = test_store();
        // Deleting a non-existent VM should not error.
        delete(&store, "nope").unwrap();
    }

    #[test]
    fn vm_with_network_round_trip() {
        let (_dir, store) = test_store();
        let mut vm = sample_vm("netvm");
        vm.network = Some(NetworkInfo {
            tap_device: "em-abc123".to_string(),
            host_ip: "10.100.0.1".to_string(),
            guest_ip: "10.100.0.2".to_string(),
            netmask: "255.255.255.252".to_string(),
            guest_mac: Some("AA:FC:00:00:00:01".to_string()),
            wan_iface: Some("eth0".to_string()),
        });
        vm.status = VmStatus::Running;
        vm.pid = Some(42);

        save(&store, &vm).unwrap();
        let loaded = load(&store, "netvm").unwrap();
        assert_eq!(loaded, vm);
        assert_eq!(loaded.network.as_ref().unwrap().guest_ip, "10.100.0.2");
    }

    #[test]
    fn json_format() {
        let vm = sample_vm("testvm");
        let json: serde_json::Value = serde_json::to_value(&vm).unwrap();

        assert_eq!(json["name"], "testvm");
        assert_eq!(json["status"], "created");
        assert_eq!(json["cpus"], 2);
        assert_eq!(json["memory_mib"], 512);
        assert_eq!(json["disk_size_gib"], 4);
        assert!(json["network"].is_null());
        assert!(json["pid"].is_null());
        assert_eq!(json["ssh"]["user"], "root");
    }

    #[test]
    fn list_skips_invalid_entries() {
        let (_dir, store) = test_store();
        save(&store, &sample_vm("good")).unwrap();

        // Create a bogus VM directory with invalid JSON.
        let bad_dir = store.root().join("vms").join("bad");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::write(bad_dir.join("vm.json"), "not json").unwrap();

        // Create a directory with no vm.json at all.
        let empty_dir = store.root().join("vms").join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();

        let vms = list(&store).unwrap();
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].name, "good");
    }
}
