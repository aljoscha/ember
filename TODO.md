# TODO

## Phase 0: Repo Setup

- [x] Update `.gitignore` for Rust
- [x] Create `Cargo.toml` with all dependencies
- [x] Create `src/main.rs` with placeholder

## Phase 1: Project Scaffold + `ember init`

- [x] Implement CLI parsing with clap derive (all commands defined, unimplemented ones return errors)
- [x] Implement unified error types
- [x] Implement root privilege check with helpful error message
- [x] Implement ZFS pool operations: create, status check (`src/zfs/pool.rs`)
- [x] Implement ZFS dataset operations: create, destroy, list (`src/zfs/dataset.rs`)
- [x] Implement file-based JSON state store with flock (`src/state/store.rs`)
- [x] Implement `ember init`: create/verify pool, create datasets, download kernel, write config
- [x] Test: `ember init` with both new pool and existing pool

## Phase 2: Image Pull + ZFS Rootfs

- [x] Implement OCI image pull (oci-unpack with skopeo fallback) (`src/image/pull.rs`)
- [x] Implement ext4 rootfs creation: mkfs, loop mount, copy content (`src/image/ext4.rs`)
- [x] Inject SSH authorized_keys and `/etc/resolv.conf` into rootfs
- [x] Implement ZFS zvol operations: create, destroy (`src/zfs/volume.rs`)
- [x] Implement image-to-zvol pipeline: dd ext4 image to zvol, create @base snapshot
- [x] Implement local image registry tracking (`src/image/registry.rs`)
- [x] Implement `ember image pull`, `image list`, `image delete`
- [x] Test: pull an image, verify ZFS snapshot exists, list shows it

## Phase 3: Basic VM Lifecycle

- [x] Implement Firecracker API client over Unix socket (`src/firecracker/api.rs`)
- [x] Implement Firecracker process management: spawn, wait, kill (`src/firecracker/process.rs`)
- [x] Implement VM config builder: translate user config to API calls (`src/firecracker/config.rs`)
- [x] Implement VM metadata types and state tracking (`src/state/vm.rs`)
- [x] Implement `ember vm create`: ZFS clone from image snapshot, loop-mount zvol to inject per-VM SSH key, write metadata
- [x] Implement `ember vm start`: spawn firecracker, configure, boot (no networking yet)
- [x] Implement `ember vm stop`: graceful shutdown + SIGKILL fallback
- [x] Implement `ember vm delete`: cleanup everything
- [x] Implement `ember vm list` and `ember vm inspect`
- [x] Test: create VM, start it, verify firecracker process running, stop, delete

## Phase 4: Networking

- [x] Implement TAP device creation via ioctl (`src/network/tap.rs`)
- [x] Implement IP allocation from /16 pool in /30 blocks (`src/network/ip.rs`)
- [x] Implement iptables NAT/masquerade rule management (`src/network/nat.rs`)
- [x] Implement WAN interface detection (`ip route get 8.8.8.8`)
- [x] Integrate networking into VM start: create TAP, allocate IP, set iptables, configure kernel `ip=` param
- [x] Integrate cleanup into VM stop/delete: remove iptables rules, delete TAP, release IP
- [x] Implement state reconciliation: cleanup orphaned TAP devices on startup
- [x] Test: start VM, verify SSH reachable from host, verify internet from guest

## Phase 5: SSH Exec + File Copy

- [x] Implement SSH client connection with retry/backoff (`src/ssh/client.rs`)
- [x] Implement `ember exec`: remote command execution with streaming I/O (`src/ssh/exec.rs`)
- [x] Implement `ember cp`: SCP-style bidirectional file transfer (`src/ssh/copy.rs`)
- [x] Implement `ember vm ssh`: convenience wrapper for interactive SSH
- [x] Test: exec a command, copy a file in both directions

## Phase 6: ZFS Snapshots

- [x] Implement ZFS snapshot operations: create, rollback, list, destroy (`src/zfs/snapshot.rs`)
- [x] Implement `ember snapshot create`
- [x] Implement `ember snapshot restore` (enforce VM stopped)
- [x] Implement `ember snapshot list`
- [x] Implement `ember snapshot delete`
- [x] Test: snapshot, modify VM, restore, verify original state

## Phase 7: VM Resize

- [x] Implement `ember vm resize`: grow zvol + resize2fs (enforce VM stopped, grow-only)
- [x] Add `--disk-size` flag to `vm resize` CLI command
- [x] Test: resize a stopped VM, start it, verify new disk size visible in guest

## Phase 8: Pause/Resume + YAML Config + Polish

- [x] Implement `ember vm pause` via Firecracker `PATCH /vm`
- [x] Implement `ember vm resume` via Firecracker `PATCH /vm`
- [x] Implement YAML config file loading and merge with CLI flags (`src/config/vm.rs`)
- [x] Add `--vm-config` flag to `vm create`
- [x] Add `--format json` output to all list/inspect commands
- [x] Add integration tests for `ember vm pause` and `ember vm resume`
- [x] Implement cleanup/rollback for partial operations (e.g., TAP created but firecracker failed)
- [x] Polish error messages across the board

## Simplification / Code Quality

Findings from a full-codebase review. Work through these one at a time.

### Bugs / Silent Misconfigurations

- [x] `--network` subnet and `network.subnet` YAML config are resolved but never wired to `start()` — silently ignored (`cli/vm.rs:251-253` resolved, `cli/vm.rs:522` uses `DEFAULT_SUBNET`)
- [x] `force_delete_vm` in `cli/image.rs:370-408` duplicates `cli/vm.rs::delete()` but skips network cleanup — VMs force-deleted during `image delete` leak TAP devices and iptables rules
- [x] `boot_args` YAML config field parsed but never consumed — silently ignored (`config/vm.rs:39`, never used in `resolve_create_config`)

### Code Deduplication — High Impact

- [x] Extract shared `force_delete_vm` function (fixes the network cleanup bug above) — `cli/image.rs:370-408` vs `cli/vm.rs:937-1001`
- [x] Extract shared `cleanup_network()` — identical in `cli/vm.rs:616-628` and `state/reconcile.rs:116-128`
- [x] Deduplicate zvol-to-image pipeline in `pull`/`build` (~25 lines each) — `cli/image.rs:141-168` and `cli/image.rs:239-266`
- [x] Extract "require running VM with network" helper — triplicated in `cli/vm.rs:1091-1108`, `cli/exec.rs:25-42`, `cli/cp.rs:42-59`
- [x] Deduplicate `now_iso8601()` — identical in `state/vm.rs:197-206` and `image/registry.rs:127-139`; also replace `date` shelling with Rust-native formatting
- [x] Deduplicate `umount()` — identical in `image/ext4.rs:127-137` and `cli/vm.rs:1177-1188`

### Code Deduplication — Lower Impact

- [x] Extract `parse_zfs_u64()` helper — same closure copy-pasted 6× across `zfs/pool.rs`, `zfs/dataset.rs`, `zfs/volume.rs`, `zfs/snapshot.rs`
- [x] Deduplicate `dataset::destroy()` and `volume::destroy()` — identical `zfs destroy` wrappers
- [x] Deduplicate shell-quoting — `cli/exec.rs:70-82` reimplements `ssh/copy.rs:370-372`
- [x] Deduplicate SSH key + resolv.conf injection in `cli/image.rs` pull vs build
- [ ] Extract "require VM stopped" helper — `cli/vm.rs:874-888` and `cli/snapshot.rs:225-239`
- [ ] Add `GlobalConfig::images_dataset()` / `vms_dataset()` helpers — path formatting repeated in `cli/image.rs` and `cli/vm.rs`
- [ ] Use `anyhow::Context` consistently instead of `map_err(|e| anyhow::anyhow!(...))` — ~5-7 sites in `firecracker/` and `cli/`

### Efficiency

- [ ] Stream SSH file transfers instead of buffering entire files in memory — `ssh/copy.rs` upload/download/upload_dir/download_dir all read into `Vec<u8>`
- [ ] Replace `format_epoch()` date shelling with Rust-native formatting — spawns a `date` process per snapshot row (`cli/snapshot.rs:153-162`)
- [ ] Process OCI whiteouts once after all layers instead of per-layer `find` scan — `image/pull.rs:198-201, 306-356`
- [ ] Batch `udevadm settle` in `image delete --force` — currently called per-VM in loop (`cli/image.rs:348-387`)

### Dead Code / Unused Config

- [ ] `Cli.log_level` and `Cli.config_file` parsed but never read (`cli/mod.rs:20-26`)
- [ ] `VmConfig.name` parsed from YAML but never used (`config/vm.rs:23`)
- [x] `ResolvedVmCreate.network` resolved but never consumed (related to network bug above)
- [ ] Several public ZFS/tap functions never called: `dataset::info/list/destroy`, `volume::info/list`, `tap::exists`
- [ ] Redundant `gateway_ip` field — always equal to `host_ip` (`network/ip.rs:37,102`)

### Minor Quality

- [ ] Extract `BASE_SNAPSHOT_NAME` constant for magic string `"base"` used in 5+ locations
- [ ] Add `From<NetworkInfo>` for `VmNetworkConfig` to eliminate manual field copying (`cli/vm.rs:658-665`)
- [ ] Avoid double-loading `ImageRegistry` in image commands (`cli/image.rs`)
