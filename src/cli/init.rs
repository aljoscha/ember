use std::path::{Path, PathBuf};

use clap::Args;

use crate::backend::{init_storage, CurrentPlatform, InitConfig, Platform};
use ember_core::config::size::ByteSize;
use ember_core::config::{derive_instance_id, DmThinMode, GlobalConfig, StorageKind, VdoConfig};
use ember_core::state::store::StateStore;

/// dm-thin pool block size (in 512-byte sectors) used when the user does
/// not pass `--block-size`. Resolved here at init time and persisted on
/// `GlobalConfig` so the value the running pool was created with stays
/// stable across ember upgrades — block size is permanent at pool
/// creation, and silently switching defaults later would orphan
/// existing pools.
#[cfg(target_os = "linux")]
const DM_THIN_DEFAULT_BLOCK_SIZE_SECTORS: u32 =
    ember_linux::dm_thin::pool::DEFAULT_BLOCK_SIZE_SECTORS;
#[cfg(not(target_os = "linux"))]
const DM_THIN_DEFAULT_BLOCK_SIZE_SECTORS: u32 = 128;

/// Convert a CLI `--block-size` byte value into the 512-byte sector
/// count the kernel expects, validating dm-thin's constraints: the
/// block size must be a multiple of 64 KiB and fit in `u32` sectors.
fn resolve_dm_thin_block_size_sectors(user: Option<ByteSize>) -> anyhow::Result<u32> {
    let Some(size) = user else {
        return Ok(DM_THIN_DEFAULT_BLOCK_SIZE_SECTORS);
    };
    let bytes = size.bytes();
    const MIN_BYTES: u64 = 64 * 1024;
    if bytes < MIN_BYTES || bytes % MIN_BYTES != 0 {
        anyhow::bail!(
            "--block-size must be at least 64K and a multiple of 64K (got {bytes} bytes)"
        );
    }
    let sectors = bytes / 512;
    u32::try_from(sectors)
        .map_err(|_| anyhow::anyhow!("--block-size {bytes} bytes overflows u32 sectors"))
}

#[derive(Args)]
pub struct InitArgs {
    /// Storage backend: zfs (default) or dm-thin (Linux only)
    #[cfg_attr(target_os = "macos", arg(long, default_value = "zfs", hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long, default_value = "zfs"))]
    pub storage: StorageKind,

    /// ZFS pool name (--storage zfs only)
    #[cfg_attr(target_os = "macos", arg(long, default_value = "ember", hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long, default_value = "ember"))]
    pub pool: String,

    /// Block device for ZFS pool creation (--storage zfs only)
    #[cfg_attr(target_os = "macos", arg(long, hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long))]
    pub device: Option<String>,

    /// Dataset name within the pool (--storage zfs only)
    #[cfg_attr(target_os = "macos", arg(long, default_value = "ember", hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long, default_value = "ember"))]
    pub dataset: String,

    /// Backing path for non-ZFS backends (directory or block device).
    ///
    /// dm-thin: directory holding metadata.img/data.img, or a raw block
    /// device. Defaults to /var/lib/ember/dm-thin when omitted.
    #[arg(long)]
    pub storage_path: Option<PathBuf>,

    /// Pool size for file-backed dm-thin (e.g. `50G`). Required when
    /// `--storage-path` is a file path; ignored for raw block devices.
    #[arg(long)]
    pub size: Option<ByteSize>,

    /// Override metadata device size for dm-thin (e.g. `800M`).
    /// `thin_metadata_size` computes a recommended value when omitted.
    #[arg(long)]
    pub metadata_size: Option<ByteSize>,

    /// dm-thin pool block size (e.g. `64K`, `1M`). Must be a multiple
    /// of 64 KiB; permanent at pool creation. Defaults to 64 KiB.
    #[arg(long)]
    pub block_size: Option<ByteSize>,

    /// Put a dm-vdo compression layer under the dm-thin pool
    /// (--storage dm-thin only). Permanent at pool creation.
    #[cfg_attr(target_os = "macos", arg(long, hide = true))]
    #[cfg_attr(not(target_os = "macos"), arg(long))]
    pub vdo: bool,

    /// Data capacity a --vdo pool hands out (e.g. `600G`). Defaults to
    /// --size, so compression shrinks the pool's real disk footprint
    /// rather than promising space that may not exist. Setting it
    /// higher over-provisions: if compression underdelivers, the pool
    /// goes read-only when the disk underneath fills.
    #[cfg_attr(target_os = "macos", arg(long, hide = true, requires = "vdo"))]
    #[cfg_attr(not(target_os = "macos"), arg(long, requires = "vdo"))]
    pub vdo_logical_size: Option<ByteSize>,

    /// Also deduplicate on a --vdo pool. Costs RAM proportional to pool
    /// size, and folds deduplication into the single savings figure
    /// `ember storage usage` reports as compression.
    #[cfg_attr(target_os = "macos", arg(long, hide = true, requires = "vdo"))]
    #[cfg_attr(not(target_os = "macos"), arg(long, requires = "vdo"))]
    pub vdo_dedup: bool,

    /// Kernel preset or file path [presets: stock]
    #[arg(long)]
    pub kernel: Option<ember_core::kernel::KernelSpec>,

    /// WAN interface for NAT (auto-detected if not specified)
    #[arg(long)]
    pub wan_iface: Option<String>,

    /// Per-installation namespace, embedded in dm-thin pool name, TAP
    /// devices, and iptables rules so two ember installations on the
    /// same host don't clash. 4 hex chars; auto-derived from a hash of
    /// the state directory when omitted.
    #[arg(long, value_parser = parse_instance_id)]
    pub instance_id: Option<String>,

    /// IPv4 base subnet handed out as /30 links to VMs (e.g.
    /// `10.42.0.0/16`). Defaults to a `/16` slot inside `10.0.0.0/8`
    /// derived from the instance id, so two installs get
    /// non-overlapping ranges automatically.
    #[arg(long)]
    pub ip_subnet: Option<String>,
}

/// Resolve the dm-vdo layer's sizes, or `None` when it was not asked
/// for.
///
/// The physical size is whatever the pool's data device will be, which
/// the dm-thin backend already knows how to work out: `--size` for a
/// file-backed pool, the device's own size for a raw one. The logical
/// size defaults to match it, which is the configuration that does not
/// over-provision.
#[cfg(target_os = "linux")]
fn resolve_vdo(
    args: &InitArgs,
    storage_path: Option<&Path>,
    mode: Option<DmThinMode>,
    state_dir: &Path,
) -> anyhow::Result<Option<VdoConfig>> {
    if !args.vdo {
        return Ok(None);
    }
    let (Some(storage_path), Some(mode)) = (storage_path, mode) else {
        anyhow::bail!("--vdo requires --storage dm-thin");
    };
    // Both sizes are rounded to a whole VDO block. `vdoformat` records
    // the volume's geometry in 4 KiB blocks, and a table built from a
    // size that is not a whole block claims a sector the volume does
    // not have, which the kernel rejects with a bare EINVAL.
    let physical_size = ember_linux::vdo::align_down(
        ember_linux::dm_thin_storage::pool_size_at_init(storage_path, state_dir, mode, args.size)?,
    );
    let logical_size = args
        .vdo_logical_size
        .map(|s| ember_linux::vdo::align_down(s.bytes()))
        .unwrap_or(physical_size);
    ember_linux::vdo::check_logical_size(logical_size)?;
    Ok(Some(VdoConfig {
        physical_size,
        logical_size,
        deduplication: args.vdo_dedup,
    }))
}

/// dm-vdo is a Linux device-mapper target, so asking for it anywhere
/// else is an error rather than a silently ignored flag.
#[cfg(not(target_os = "linux"))]
fn resolve_vdo(
    args: &InitArgs,
    _storage_path: Option<&Path>,
    _mode: Option<DmThinMode>,
    _state_dir: &Path,
) -> anyhow::Result<Option<VdoConfig>> {
    if args.vdo {
        anyhow::bail!("--vdo is a Linux dm-thin feature and is not available on this platform");
    }
    Ok(None)
}

/// Validate `--instance-id`: 4 lowercase hex chars (uppercase is folded).
fn parse_instance_id(s: &str) -> Result<String, String> {
    let lower = s.to_ascii_lowercase();
    if lower.len() != 4 || !lower.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "--instance-id must be exactly 4 hex chars (got {s:?})"
        ));
    }
    Ok(lower)
}

pub fn run(args: &InitArgs, state_dir: &Path) -> anyhow::Result<()> {
    // Refuse to touch an installation that already exists. This has to
    // happen before `init_storage`, which zeroes the thin metadata
    // superblock and would destroy every VM and image in the pool long
    // before `store.create` got around to reporting the conflict.
    let store = StateStore::new(state_dir.to_path_buf());
    // A config that exists but will not parse is still an existing
    // installation, and the one thing we must not do is treat it as a
    // fresh one. A genuinely fresh state dir yields `Ok(None)`.
    let existing = store
        .read_optional::<GlobalConfig>(&store.config_path())
        .map_err(|e| {
            anyhow::anyhow!(
                "{} exists but could not be read: {e}. Refusing to initialize over it, \
                 because that would destroy any pool it describes. Fix or remove the \
                 file, or run 'ember deinit'.",
                store.config_path().display(),
            )
        })?;
    if let Some(existing) = existing {
        if existing.storage_backend != args.storage {
            anyhow::bail!(
                "ember is already initialized with the {:?} backend. \
                 Run 'ember deinit' first to switch to {:?}.",
                existing.storage_backend,
                args.storage,
            );
        }
        if existing.vdo.is_some() != args.vdo {
            anyhow::bail!(
                "ember is already initialized {} a compression layer. It cannot be {} \
                 an existing pool without rewriting every block, so run \
                 'ember deinit' first.",
                if existing.vdo.is_some() {
                    "with"
                } else {
                    "without"
                },
                if args.vdo { "added to" } else { "removed from" },
            );
        }
        anyhow::bail!(
            "ember is already initialized at {} — run 'ember deinit' first to reconfigure",
            store.config_path().display(),
        );
    }

    if args.vdo && args.storage != StorageKind::DmThin {
        anyhow::bail!(
            "--vdo puts a compression layer under the dm-thin pool's data device, \
             so it requires --storage dm-thin (got {:?})",
            args.storage,
        );
    }

    // Resolve the dm-thin defaults so both InitConfig and GlobalConfig
    // see the same values.
    let storage_path = match args.storage {
        StorageKind::DmThin => Some(
            args.storage_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/var/lib/ember/dm-thin")),
        ),
        StorageKind::Btrfs => args.storage_path.clone(),
        StorageKind::Zfs => None,
    };

    // Resolve block size up-front for dm-thin so the persisted config
    // pins the value the pool was actually created with, even when the
    // user omits `--block-size`. Internally the kernel addresses pool
    // blocks in 512-byte sectors; the CLI accepts a `ByteSize` so the
    // UX matches `--size` / `--metadata-size`.
    let resolved_block_size = match args.storage {
        StorageKind::DmThin => Some(resolve_dm_thin_block_size_sectors(args.block_size)?),
        _ => None,
    };

    // Resolve file-vs-raw-device layout once and persist it. Doing this
    // here rather than in the backend keeps the contract explicit:
    // reactivation should not depend on a live `is_dir()` probe of
    // `storage_path` agreeing with what init saw.
    let resolved_dm_thin_mode = match (args.storage, storage_path.as_ref()) {
        (StorageKind::DmThin, Some(path)) => {
            if path.is_dir() || !path.exists() {
                Some(DmThinMode::File)
            } else {
                Some(DmThinMode::RawDevice)
            }
        }
        _ => None,
    };

    // Same reasoning for the VDO layer: its sizes end up in both
    // structs, and the kernel refuses to activate a volume whose
    // recorded sizes disagree with how it was formatted.
    let resolved_vdo = resolve_vdo(
        args,
        storage_path.as_deref(),
        resolved_dm_thin_mode,
        state_dir,
    )?;

    // Resolve instance_id and ip_subnet up-front so InitConfig and the
    // persisted GlobalConfig agree. dm-thin in particular needs the
    // instance id during init to name the kernel pool.
    let instance_id = args
        .instance_id
        .clone()
        .unwrap_or_else(|| derive_instance_id(state_dir));
    // Default subnet is platform-derived: Linux carves up 10.0.0.0/8,
    // macOS sub-allocates inside vmnet's host-wide 192.168.64.0/24.
    let ip_subnet = args
        .ip_subnet
        .clone()
        .unwrap_or_else(|| CurrentPlatform::default_ip_subnet(&instance_id));

    let init_config = InitConfig {
        storage_backend: args.storage,
        state_dir: state_dir.to_path_buf(),
        instance_id: instance_id.clone(),
        pool: args.pool.clone(),
        dataset: args.dataset.clone(),
        device: args.device.clone(),
        storage_path: storage_path.clone(),
        btrfs_size: None,
        dm_thin_size: args.size,
        dm_thin_metadata_size: args.metadata_size,
        dm_thin_block_size: resolved_block_size,
        dm_thin_mode: resolved_dm_thin_mode,
        vdo: resolved_vdo,
    };
    init_storage(&init_config)?;

    // Initialize state directory structure.
    store.init()?;
    println!("State directory initialized at {}", state_dir.display());

    // Download kernel if preset or path provided.
    let kernel_path = if let Some(spec) = &args.kernel {
        Some(spec.resolve(&store)?)
    } else {
        println!("No --kernel provided; a default kernel will be downloaded on first 'vm create'.");
        None
    };

    // Detect or use provided WAN interface.
    let (wan_iface, messages) = CurrentPlatform::detect_wan_iface(args.wan_iface.as_deref());
    for msg in &messages {
        println!("{msg}");
    }

    // Write config.
    let config = GlobalConfig {
        storage_backend: args.storage,
        pool: args.pool.clone(),
        dataset: args.dataset.clone(),
        kernel_path,
        wan_iface,
        state_dir: state_dir.to_path_buf(),
        instance_id: instance_id.clone(),
        ip_subnet: ip_subnet.clone(),
        storage_path,
        dm_thin_block_size: resolved_block_size,
        dm_thin_mode: resolved_dm_thin_mode,
        vdo: resolved_vdo,
    };
    store
        .create(&store.config_path(), &config)
        .map_err(|e| match e {
            ember_core::error::Error::AlreadyExists { .. } => anyhow::anyhow!(
                "ember is already initialized at {} — run 'ember deinit' first to reconfigure",
                store.config_path().display()
            ),
            other => other.into(),
        })?;
    println!("Configuration written to {}", store.config_path().display());
    println!("Instance id: {instance_id}");
    println!("VM IP subnet: {ip_subnet}");

    println!("\nember initialized successfully.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ember_core::config::StorageKind;
    use std::path::PathBuf;

    fn zfs_config(pool: &str, dataset: &str) -> GlobalConfig {
        GlobalConfig {
            storage_backend: StorageKind::Zfs,
            pool: pool.to_string(),
            dataset: dataset.to_string(),
            kernel_path: None,
            wan_iface: None,
            state_dir: PathBuf::default(),
            instance_id: "abcd".to_string(),
            ip_subnet: "10.100.0.0/16".to_string(),
            storage_path: None,
            dm_thin_block_size: None,
            dm_thin_mode: None,
            vdo: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn vdo_args(vdo: bool, logical: Option<&str>, dedup: bool) -> InitArgs {
        InitArgs {
            storage: StorageKind::DmThin,
            pool: "ember".to_string(),
            device: None,
            dataset: "ember".to_string(),
            storage_path: None,
            size: Some("64G".parse().unwrap()),
            metadata_size: None,
            block_size: None,
            vdo,
            vdo_logical_size: logical.map(|s| s.parse().unwrap()),
            vdo_dedup: dedup,
            kernel: None,
            wan_iface: None,
            instance_id: None,
            ip_subnet: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn resolve(args: &InitArgs) -> anyhow::Result<Option<VdoConfig>> {
        resolve_vdo(
            args,
            Some(Path::new("/var/lib/ember/dm-thin")),
            Some(DmThinMode::File),
            Path::new("/var/lib/ember"),
        )
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn no_vdo_flag_means_no_layer() {
        assert_eq!(resolve(&vdo_args(false, None, false)).unwrap(), None);
    }

    /// The default is the configuration that does not over-promise:
    /// compression buys a smaller footprint, not extra capacity.
    #[test]
    #[cfg(target_os = "linux")]
    fn logical_size_defaults_to_the_physical_size() {
        let cfg = resolve(&vdo_args(true, None, false)).unwrap().unwrap();
        assert_eq!(cfg.physical_size, 64 << 30);
        assert_eq!(cfg.logical_size, cfg.physical_size);
        assert!(!cfg.deduplication);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn logical_size_can_over_provision_explicitly() {
        let cfg = resolve(&vdo_args(true, Some("128G"), true))
            .unwrap()
            .unwrap();
        assert_eq!(cfg.physical_size, 64 << 30);
        assert_eq!(cfg.logical_size, 128 << 30);
        assert!(cfg.deduplication);
    }

    /// Both sizes must be whole VDO blocks, or the table claims a
    /// sector the formatted volume does not have.
    #[test]
    #[cfg(target_os = "linux")]
    fn sizes_are_rounded_to_whole_vdo_blocks() {
        // Both a hair over a whole number of blocks, and both well
        // above the minimum so the floor is not what is being tested.
        let mut args = vdo_args(true, Some("33554433K"), false);
        args.size = Some("67108865K".parse().unwrap());
        let cfg = resolve(&args).unwrap().unwrap();
        assert_eq!(cfg.physical_size, 64 << 30);
        assert_eq!(cfg.logical_size, 32 << 30);
    }

    /// A logical size small enough to round down to zero would mean
    /// "match the physical size" to `vdoformat` and a zero-length
    /// device in the table.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_logical_size_below_the_floor_is_refused() {
        assert!(resolve(&vdo_args(true, Some("1M"), false)).is_err());
        assert!(resolve(&vdo_args(true, Some("1K"), false)).is_err());
    }

    /// A file-backed pool has no size to discover, so `--vdo` without
    /// `--size` is an error rather than a guess.
    #[test]
    #[cfg(target_os = "linux")]
    fn vdo_without_a_size_on_a_file_pool_is_an_error() {
        let mut args = vdo_args(true, None, false);
        args.size = None;
        assert!(resolve(&args).is_err());
    }

    #[test]
    fn global_config_round_trip_with_kernel() {
        let config = GlobalConfig {
            kernel_path: Some(PathBuf::from("/var/lib/ember/kernels/vmlinux")),
            wan_iface: Some("eth0".to_string()),
            state_dir: PathBuf::from("/var/lib/ember"),
            ..zfs_config("testpool", "ember")
        };

        let json = serde_json::to_string(&config).unwrap();
        let loaded: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn global_config_round_trip_without_kernel() {
        let config = zfs_config("mypool", "mydata");
        let json = serde_json::to_string(&config).unwrap();
        let loaded: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn global_config_json_format() {
        let config = GlobalConfig {
            kernel_path: Some(PathBuf::from("/kernels/vmlinux")),
            wan_iface: Some("wlp2s0".to_string()),
            ..zfs_config("tank", "ember")
        };

        let json: serde_json::Value = serde_json::to_value(&config).unwrap();
        assert_eq!(json["pool"], "tank");
        assert_eq!(json["dataset"], "ember");
        assert_eq!(json["kernel_path"], "/kernels/vmlinux");
        assert_eq!(json["wan_iface"], "wlp2s0");
        assert_eq!(json["storage_backend"], "zfs");
    }

    #[test]
    fn global_config_null_kernel_in_json() {
        let config = zfs_config("tank", "ember");
        let json: serde_json::Value = serde_json::to_value(&config).unwrap();
        assert!(json["kernel_path"].is_null());
    }

    #[test]
    fn global_config_written_to_state_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();

        let config = GlobalConfig {
            wan_iface: Some("eth0".to_string()),
            state_dir: dir.path().to_path_buf(),
            ..zfs_config("testpool", "ember")
        };
        store.create(&store.config_path(), &config).unwrap();

        let loaded: GlobalConfig = store.read(&store.config_path()).unwrap();
        assert_eq!(loaded.pool, "testpool");
        assert_eq!(loaded.dataset, "ember");
        assert_eq!(loaded.kernel_path, None);
        assert_eq!(loaded.wan_iface, Some("eth0".to_string()));
    }

    #[test]
    fn global_config_create_rejects_reinit() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();

        let config1 = GlobalConfig {
            wan_iface: Some("eth0".to_string()),
            state_dir: dir.path().to_path_buf(),
            ..zfs_config("pool1", "ds1")
        };
        store.create(&store.config_path(), &config1).unwrap();

        // A second create must fail rather than clobber the existing config.
        let config2 = GlobalConfig {
            state_dir: dir.path().to_path_buf(),
            ..zfs_config("pool2", "ds2")
        };
        let err = store.create(&store.config_path(), &config2).unwrap_err();
        assert!(matches!(
            err,
            ember_core::error::Error::AlreadyExists { .. }
        ));

        // The original config is untouched.
        let loaded: GlobalConfig = store.read(&store.config_path()).unwrap();
        assert_eq!(loaded, config1);
    }

    #[test]
    fn global_config_kernel_path_updated_via_delta() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().to_path_buf());
        store.init().unwrap();

        let config = GlobalConfig {
            state_dir: dir.path().to_path_buf(),
            ..zfs_config("tank", "ember")
        };
        store.create(&store.config_path(), &config).unwrap();

        store
            .update(&store.config_path(), |c: &mut GlobalConfig| {
                c.kernel_path = Some(PathBuf::from("/kernels/vmlinux"));
                Ok(())
            })
            .unwrap();

        let loaded: GlobalConfig = store.read(&store.config_path()).unwrap();
        assert_eq!(loaded.kernel_path, Some(PathBuf::from("/kernels/vmlinux")));
        assert_eq!(loaded.pool, "tank");
    }

    #[test]
    fn global_config_backwards_compatible_without_wan_iface() {
        // Older config.json files won't have wan_iface or storage_backend
        // — serde(default) handles both.
        let json = r#"{"pool":"tank","dataset":"ember","kernel_path":null}"#;
        let loaded: GlobalConfig = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.pool, "tank");
        assert_eq!(loaded.wan_iface, None);
        assert_eq!(loaded.storage_backend, StorageKind::Zfs);
    }
}
