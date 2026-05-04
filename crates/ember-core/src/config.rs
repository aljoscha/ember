pub mod size;
pub mod vm;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which storage backend is active.
///
/// On Linux, runtime-selected at `ember init` and serialized to `config.json`.
/// Older configs without this field default to [`StorageKind::Zfs`] for
/// backwards compatibility.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StorageKind {
    #[default]
    Zfs,
    Btrfs,
    DmThin,
}

impl std::str::FromStr for StorageKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "zfs" => Ok(Self::Zfs),
            "btrfs" => Ok(Self::Btrfs),
            "dm-thin" | "dmthin" | "dm_thin" => Ok(Self::DmThin),
            other => Err(format!(
                "unknown storage backend '{other}' (expected zfs, btrfs, or dm-thin)"
            )),
        }
    }
}

/// How the dm-thin pool's data device is provided.
///
/// Resolved at `ember init` from the `--storage-path` argument and
/// persisted on `GlobalConfig` so reactivation does not depend on a
/// runtime filesystem probe — `is_dir()` could disagree with init if
/// the directory was removed, or a raw device replaced a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DmThinMode {
    /// `--storage-path` is a directory holding `metadata.img`/`data.img`.
    File,
    /// `--storage-path` is a raw block device used as the data device.
    /// Metadata then lives under `state_dir/dm-thin-metadata.img`.
    RawDevice,
}

/// Global configuration written by `ember init`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// Storage backend selected at init time.
    /// Defaults to [`StorageKind::Zfs`] for older configs without this field.
    #[serde(default)]
    pub storage_backend: StorageKind,
    pub pool: String,
    pub dataset: String,
    pub kernel_path: Option<PathBuf>,
    /// Default WAN interface for iptables NAT rules.
    /// Auto-detected during `ember init`, overridable via `--wan-iface`.
    #[serde(default)]
    pub wan_iface: Option<String>,
    /// State directory path. Used by macOS backend to derive storage paths.
    /// Populated during `ember init`; defaults to empty path for backwards compat.
    #[serde(default)]
    pub state_dir: PathBuf,
    /// Per-installation namespace, derived at `ember init` from a hash
    /// of the canonicalized state directory (or supplied via
    /// `--instance-id`). 4 hex chars; embedded in every host-global
    /// resource name (dm-thin pool, TAP devices, iptables comments) so
    /// two ember installations on the same host don't clash. Empty
    /// values are treated as a malformed config — `ember init` always
    /// pins a non-empty value.
    #[serde(default)]
    pub instance_id: String,
    /// IPv4 base subnet handed out as /30 links to VMs. Defaults at
    /// init time to `10.{slot}.0.0/16` where `slot` is derived from the
    /// instance-id hash, so two installs get non-overlapping ranges
    /// without the user having to think about it. Overridable via
    /// `--ip-subnet`.
    #[serde(default = "default_ip_subnet")]
    pub ip_subnet: String,
    /// Backing path for non-ZFS backends.
    ///
    /// * btrfs: block device or sparse image file containing the btrfs filesystem.
    /// * dm-thin: directory holding `metadata.img`/`data.img`, or a raw block device.
    /// * ZFS: unused.
    #[serde(default)]
    pub storage_path: Option<PathBuf>,
    /// dm-thin pool block size in 512-byte sectors. Permanent at pool
    /// creation, so `ember init` resolves the user flag (or default) at
    /// init time and persists the actual value here. `None` means "use
    /// the backend default" — only legacy configs predating this
    /// resolution should hit that branch; new configs always pin the
    /// value the running pool was created with.
    #[serde(default)]
    pub dm_thin_block_size: Option<u32>,
    /// dm-thin pool layout: file-backed (sparse files inside
    /// `storage_path`) or raw-device (`storage_path` is a block device).
    /// Resolved at `ember init` and persisted so reactivation does not
    /// rely on a live `is_dir()` probe. `None` on legacy configs and on
    /// non-dm-thin backends.
    #[serde(default)]
    pub dm_thin_mode: Option<DmThinMode>,
}

/// Fallback subnet used when a config predates `ip_subnet`. New
/// installs derive this at init from the instance-id hash; this value
/// only applies to deserialization of older `config.json` files.
pub fn default_ip_subnet() -> String {
    "10.100.0.0/16".to_string()
}

/// Derive a default 4-hex-char `instance_id` from the canonicalized
/// state directory path. Two installations with distinct state
/// directories almost always get distinct ids; the same state
/// directory is stable across invocations so reactivation finds the
/// resources it created at init time.
///
/// 16-bit space is small (~256-instance birthday-collision threshold),
/// but two installs on one host is a personal-use scenario; users who
/// hit a collision can pass `--instance-id` explicitly.
pub fn derive_instance_id(state_dir: &std::path::Path) -> String {
    // Canonicalize when possible so `/var/lib/ember` and
    // `/var/lib/ember/` hash to the same id; fall back to the literal
    // path bytes when the directory does not yet exist.
    let canonical = state_dir
        .canonicalize()
        .unwrap_or_else(|_| state_dir.to_path_buf());
    let bytes = canonical.as_os_str().as_encoded_bytes();
    format!("{:04x}", fnv1a_32(bytes) as u16)
}

/// Derive a default `/16` IPv4 subnet from an instance id, chosen so
/// two installations rarely overlap: `10.{slot}.0.0/16` where `slot`
/// is the high byte of the same FNV-1a hash that produced the id. The
/// /16 still gives ~16k VMs per install via /30 links.
pub fn derive_ip_subnet(instance_id: &str) -> String {
    let hash = fnv1a_32(instance_id.as_bytes());
    let slot = ((hash >> 8) & 0xff) as u8;
    format!("10.{slot}.0.0/16")
}

/// FNV-1a 32-bit hash. Stable across Rust versions (unlike
/// `DefaultHasher`) and small enough to inline rather than pulling in
/// a crypto dep just for non-security-critical name derivation.
fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

impl GlobalConfig {
    /// Full ZFS dataset path for images (e.g. `ember/ember/images`).
    pub fn images_dataset(&self) -> String {
        format!("{}/{}/images", self.pool, self.dataset)
    }

    /// Full ZFS dataset path for VMs (e.g. `ember/ember/vms`).
    pub fn vms_dataset(&self) -> String {
        format!("{}/{}/vms", self.pool, self.dataset)
    }

    /// dm-thin pool name. New installs get `ember-{instance_id}-pool`;
    /// configs that predate `instance_id` (empty value) keep the
    /// legacy unprefixed `ember-pool` so existing kernel state on
    /// upgraded hosts is still reachable.
    pub fn dm_thin_pool_name(&self) -> String {
        if self.instance_id.is_empty() {
            // Older binaries created `dmsetup create ember-pool`. Any
            // other string here would point at a non-existent pool
            // (or, worse, a `ember--pool` typo that init would race
            // to create), orphaning the data on disk.
            "ember-pool".to_string()
        } else {
            format!("ember-{}-pool", self.instance_id)
        }
    }

    /// Device-mapper name prefix for image base volumes
    /// (`ember-{id}-img-`, or legacy `ember-img-`).
    pub fn dm_thin_image_prefix(&self) -> String {
        if self.instance_id.is_empty() {
            // Existing image volumes on the host are named
            // `ember-img-<image>`; teardown's prefix sweep and
            // per-image lookups must match that exact prefix or the
            // pool will accumulate orphaned thin ids.
            "ember-img-".to_string()
        } else {
            format!("ember-{}-img-", self.instance_id)
        }
    }

    /// Device-mapper name prefix for VM disks (`ember-{id}-vm-`, or
    /// legacy `ember-vm-`).
    pub fn dm_thin_vm_prefix(&self) -> String {
        if self.instance_id.is_empty() {
            // Same reasoning as `dm_thin_image_prefix`: existing VM
            // disks are `ember-vm-<vm>`, and `vm.json` records the
            // device path with that prefix.
            "ember-vm-".to_string()
        } else {
            format!("ember-{}-vm-", self.instance_id)
        }
    }

    /// TAP device name prefix (`em{id}-`, or legacy `em-`). Bounded so
    /// `prefix + 7-hex VM id` fits in Linux's 15-char `IFNAMSIZ - 1`
    /// budget (14 chars with the 4-hex id, 10 chars in legacy mode).
    pub fn tap_prefix(&self) -> String {
        if self.instance_id.is_empty() {
            // Older binaries persisted TAP names like `em-<vmid7>` on
            // every running VM's `vm.json`; reconcile's orphan sweep
            // and teardown's `ip link delete` both reference that
            // exact name, so the prefix has to stay 3 chars.
            "em-".to_string()
        } else {
            format!("em{}-", self.instance_id)
        }
    }

    /// Comment string embedded in iptables rules to scope cleanup to
    /// this installation. Returns `""` for legacy configs — the old
    /// binary added rules without a comment match, so the rules
    /// already on the host don't carry one to match against.
    pub fn iptables_comment(&self) -> String {
        if self.instance_id.is_empty() {
            // `iptables -D` requires a byte-for-byte rule match. If
            // we returned a non-empty comment here, the upgraded
            // binary would emit `... -m comment --comment ember ...`
            // on add and try to delete the same shape on remove —
            // but the rule already on the host has no comment match,
            // so the delete silently no-ops and rules accumulate
            // forever. Empty preserves the original rule shape.
            String::new()
        } else {
            format!("ember:{}", self.instance_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config_with_id(id: &str) -> GlobalConfig {
        GlobalConfig {
            storage_backend: StorageKind::Zfs,
            pool: "tank".to_string(),
            dataset: "ember".to_string(),
            kernel_path: None,
            wan_iface: None,
            state_dir: PathBuf::default(),
            instance_id: id.to_string(),
            ip_subnet: "10.100.0.0/16".to_string(),
            storage_path: None,
            dm_thin_block_size: None,
            dm_thin_mode: None,
        }
    }

    #[test]
    fn instance_id_derived_from_state_dir_is_4_hex_chars() {
        let id = derive_instance_id(std::path::Path::new("/var/lib/ember"));
        assert_eq!(id.len(), 4);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn instance_id_is_stable_across_calls() {
        let p = std::path::Path::new("/some/path/that/does/not/exist");
        assert_eq!(derive_instance_id(p), derive_instance_id(p));
    }

    #[test]
    fn distinct_state_dirs_usually_get_distinct_ids() {
        // 16-bit space, but two unrelated paths almost never collide.
        let a = derive_instance_id(std::path::Path::new("/var/lib/ember"));
        let b = derive_instance_id(std::path::Path::new("/tmp/ember-test"));
        assert_ne!(a, b);
    }

    #[test]
    fn derived_subnet_lands_in_10_slash_8() {
        let subnet = derive_ip_subnet("a3f4");
        assert!(subnet.starts_with("10."));
        assert!(subnet.ends_with(".0.0/16"));
    }

    #[test]
    fn dm_thin_pool_name_embeds_instance_id() {
        let cfg = config_with_id("a3f4");
        assert_eq!(cfg.dm_thin_pool_name(), "ember-a3f4-pool");
        assert_eq!(cfg.dm_thin_image_prefix(), "ember-a3f4-img-");
        assert_eq!(cfg.dm_thin_vm_prefix(), "ember-a3f4-vm-");
    }

    #[test]
    fn tap_prefix_fits_ifnamsiz_with_7_hex_vm_id() {
        let cfg = config_with_id("ffff");
        let prefix = cfg.tap_prefix();
        assert_eq!(prefix, "emffff-");
        // Linux IFNAMSIZ is 16 with NUL; usable budget is 15. The full
        // device name is `prefix + 7 hex chars` = 14 chars.
        assert!(prefix.len() + 7 <= 15);
    }

    #[test]
    fn iptables_comment_tags_install() {
        let cfg = config_with_id("a3f4");
        assert_eq!(cfg.iptables_comment(), "ember:a3f4");
    }

    /// Configs written by older ember binaries don't have `instance_id`.
    /// Serde fills the field with the empty-string default; the
    /// accessors then have to return the legacy unprefixed names, or
    /// we'd lose track of kernel state on every upgrade.
    #[test]
    fn legacy_config_without_instance_id_uses_unprefixed_names() {
        let json = r#"{"pool":"tank","dataset":"ember","kernel_path":null}"#;
        let cfg: GlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.instance_id, "");
        assert_eq!(cfg.dm_thin_pool_name(), "ember-pool");
        assert_eq!(cfg.dm_thin_image_prefix(), "ember-img-");
        assert_eq!(cfg.dm_thin_vm_prefix(), "ember-vm-");
        assert_eq!(cfg.tap_prefix(), "em-");
        assert_eq!(cfg.iptables_comment(), "");
        // ip_subnet falls back to the historical default subnet.
        assert_eq!(cfg.ip_subnet, "10.100.0.0/16");
    }
}
