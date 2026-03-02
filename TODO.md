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
- [ ] Test: snapshot, modify VM, restore, verify original state

## Phase 7: VM Resize

- [ ] Implement `ember vm resize`: grow zvol + resize2fs (enforce VM stopped, grow-only)
- [ ] Add `--disk-size` flag to `vm resize` CLI command
- [ ] Test: resize a stopped VM, start it, verify new disk size visible in guest

## Phase 8: Pause/Resume + YAML Config + Polish

- [ ] Implement `ember vm pause` via Firecracker `PATCH /vm`
- [ ] Implement `ember vm resume` via Firecracker `PATCH /vm`
- [ ] Implement YAML config file loading and merge with CLI flags (`src/config/vm.rs`)
- [ ] Add `--config` flag to `vm create`
- [ ] Add `--format json` output to all list/inspect commands
- [ ] Implement cleanup/rollback for partial operations (e.g., TAP created but firecracker failed)
- [ ] Polish error messages across the board
