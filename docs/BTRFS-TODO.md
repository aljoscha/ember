# btrfs Backend TODO

## Phase 0: Preparatory Refactors (no behavior change)

These changes make the codebase backend-agnostic without adding btrfs yet. Everything continues to work with ZFS only.

- [ ] Rename `VmMetadata.zvol_path` to `disk_path` with `#[serde(alias = "zvol_path")]` for backward compat
- [ ] Rename `ImageEntry.zvol` to `disk_path` with `#[serde(alias = "zvol")]` for backward compat
- [ ] Update all references: `cli/vm.rs`, `cli/image.rs`, `backend/linux/vm.rs`, `state/vm.rs`, `image/registry.rs`
- [ ] Update display labels in inspect commands (e.g., "ZFS zvol" → "Disk")
- [ ] Add `storage_backend` field to `GlobalConfig` (default `"zfs"`, `#[serde(default)]` for backward compat)
- [ ] Add `data_dir` field to `GlobalConfig` (`Option<PathBuf>`, `#[serde(default)]`)
- [ ] Add `btrfs_device` field to `GlobalConfig` (`Option<String>`, for remounting)
- [ ] Extend `InitConfig` with `storage_backend`, `data_dir`, `btrfs_device`, `btrfs_size`
- [ ] Verify: `cargo build && cargo test` — no behavior change, existing ZFS configs still work

## Phase 1: Runtime Storage Dispatch

Change `Storage` from a compile-time type alias to a runtime enum.

- [ ] Create `Storage` enum in `backend/mod.rs`: `Zfs(LinuxStorage) | Btrfs(BtrfsStorage)`
- [ ] Implement `StorageBackend` for `Storage` enum by delegating to inner variant
- [ ] Add `Storage::new(config: &GlobalConfig) -> Storage` factory that reads `storage_backend`
- [ ] Add dispatch in `Storage::init()` based on `InitConfig.storage_backend`
- [ ] Update `cli/init.rs`: add `--storage` flag (zfs/btrfs), `--data-dir`, `--size` to `InitArgs`
- [ ] Wire init dispatch: btrfs path validates and calls `BtrfsStorage::init`, ZFS path unchanged
- [ ] Verify: `cargo build && cargo test` — ZFS still works, btrfs init is a stub/skeleton

## Phase 2: btrfs Filesystem Management

Implement the init and mount lifecycle for btrfs filesystems.

- [ ] Implement `BtrfsStorage::init()`: format device with `mkfs.btrfs`, mount, create directories
- [ ] Support file-backed mode: `truncate -s <size>` + `mkfs.btrfs` + `mount -o loop`
- [ ] Support block device mode: `mkfs.btrfs -f <device>` + `mount`
- [ ] Enable transparent compression: all mounts use `-o compress=zstd:3`
- [ ] Validate btrfs: check filesystem type via `stat -f` or `/proc/mounts`
- [ ] Implement auto-remount: if data_dir not mounted, mount using recorded device/file path
- [ ] Add `fstab`-style mount option recording in config for reliable remounting
- [ ] Test: `ember init --storage btrfs --device /tmp/test-btrfs.img --size 2G`
- [ ] Test: verify remount after `umount`

## Phase 3: btrfs Image Storage

Implement image pull/build pipeline for btrfs.

- [ ] Implement `BtrfsStorage::create_image_volume()`: copy ext4 image to `data_dir/images/<name>.img`
- [ ] Implement `BtrfsStorage::destroy_image_storage()`: `rm data_dir/images/<name>.img`
- [ ] Wire image pull pipeline: after ext4 creation, call `create_image_volume` (copies file to btrfs)
- [ ] Wire image build pipeline: same as pull
- [ ] Test: `ember image pull alpine:latest` with btrfs backend, verify `.img` in images dir
- [ ] Test: `ember image delete`, verify file removed

## Phase 4: btrfs VM Create + Clone

Implement VM creation with reflink clones.

- [ ] Implement `BtrfsStorage::clone_for_vm()`: `mkdir -p` + `cp --reflink=always`
- [ ] Implement `BtrfsStorage::disk_device_path()`: return `data_dir/vms/<name>/rootfs.img`
- [ ] Implement `BtrfsStorage::mount()`: `mount -o loop <rootfs.img> <tmpdir>`
- [ ] Implement `BtrfsStorage::unmount()`: `umount` + remove tmpdir
- [ ] Implement `BtrfsStorage::destroy_vm_storage()`: `rm -rf data_dir/vms/<name>/`
- [ ] Update `backend/linux/vm.rs`: handle file paths in `disk_path` (starts with `/` → use directly, else → `zfs::volume::device_path`)
- [ ] Add `--reflink=always` failure detection with clear error message
- [ ] Add timing-based sanity check (warn if clone > 1s)
- [ ] Test: `ember vm create testvm --image alpine:latest`, verify reflink clone
- [ ] Test: `ember vm start testvm`, verify Firecracker boots with file-backed rootfs
- [ ] Test: `ember vm delete testvm`, verify cleanup

## Phase 5: btrfs Snapshots

Implement snapshot create/restore/list/delete.

- [ ] Implement `BtrfsStorage::snapshot()`: `mkdir -p snapshots/` + `cp --reflink=always`
- [ ] Implement `BtrfsStorage::restore_snapshot()`: reflink to `.tmp` + atomic `mv`
- [ ] Implement `BtrfsStorage::delete_snapshot()`: `rm snapshots/<name>.img`
- [ ] Implement `BtrfsStorage::list_snapshots()`: `readdir` + `stat` each `.img` for size/mtime
- [ ] Test: create snapshot, modify VM, restore, verify original state
- [ ] Test: list snapshots shows correct metadata
- [ ] Test: delete snapshot frees space

## Phase 6: btrfs VM Resize

- [ ] Implement `BtrfsStorage::resize()`: `truncate -s` + `e2fsck` + `resize2fs` on the image file
- [ ] Test: resize a stopped VM, start it, verify new disk size in guest

## Phase 7: btrfs VM Fork

- [ ] Implement `BtrfsStorage::clone_from_snapshot()`: create fork snapshot + reflink clone to target
- [ ] Implement `BtrfsStorage::destroy_fork_origin()`: `rm` the fork snapshot file from source VM
- [ ] Test: fork a VM, verify both run independently
- [ ] Test: delete forked VM, verify fork snapshot cleaned up on source
- [ ] Test: delete source VM with existing forks (should warn/refuse without --force)

## Phase 8: Polish + Integration Testing

- [ ] End-to-end test: init → image pull → vm create → ssh → exec → snapshot → restore → fork → resize → delete
- [ ] Test backward compatibility: existing ZFS config.json without `storage_backend` field
- [ ] Test backward compatibility: existing vm.json with `zvol_path` field
- [ ] Test: `ember init --storage btrfs` without `--device` gives helpful error
- [ ] Test: `cp --reflink=always` on non-btrfs gives clear error
- [ ] Update CLAUDE.md with btrfs backend notes
- [ ] Add btrfs build/test commands to CLAUDE.md
