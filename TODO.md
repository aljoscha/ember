# TODO

## Phase 0: Repo Setup

- [x] Update `.gitignore` for Rust
- [x] Create `Cargo.toml` with all dependencies
- [x] Create `src/main.rs` with placeholder

## Phase 1: Project Scaffold + `crackling init`

- [x] Implement CLI parsing with clap derive (all commands defined, unimplemented ones return errors)
- [x] Implement unified error types
- [x] Implement root privilege check with helpful error message
- [x] Implement ZFS pool operations: create, status check (`src/zfs/pool.rs`)
- [x] Implement ZFS dataset operations: create, destroy, list (`src/zfs/dataset.rs`)
- [x] Implement file-based JSON state store with flock (`src/state/store.rs`)
- [x] Implement `crackling init`: create/verify pool, create datasets, download kernel, write config
- [x] Test: `crackling init` with both new pool and existing pool

## Phase 2: Image Pull + ZFS Rootfs

- [x] Implement OCI image pull (oci-unpack with skopeo fallback) (`src/image/pull.rs`)
- [x] Implement ext4 rootfs creation: mkfs, loop mount, copy content (`src/image/ext4.rs`)
- [x] Inject SSH authorized_keys and `/etc/resolv.conf` into rootfs
- [x] Implement ZFS zvol operations: create, destroy (`src/zfs/volume.rs`)
- [ ] Implement image-to-zvol pipeline: dd ext4 image to zvol, create @base snapshot
- [ ] Implement local image registry tracking (`src/image/registry.rs`)
- [ ] Implement `crackling image pull`, `image list`, `image delete`
- [ ] Test: pull an image, verify ZFS snapshot exists, list shows it

## Phase 3: Basic VM Lifecycle

- [ ] Implement Firecracker API client over Unix socket (`src/firecracker/api.rs`)
- [ ] Implement Firecracker process management: spawn, wait, kill (`src/firecracker/process.rs`)
- [ ] Implement VM config builder: translate user config to API calls (`src/firecracker/config.rs`)
- [ ] Implement VM metadata types and state tracking (`src/state/vm.rs`)
- [ ] Implement `crackling vm create`: ZFS clone from image snapshot, write metadata
- [ ] Implement `crackling vm start`: spawn firecracker, configure, boot (no networking yet)
- [ ] Implement `crackling vm stop`: graceful shutdown + SIGKILL fallback
- [ ] Implement `crackling vm delete`: cleanup everything
- [ ] Implement `crackling vm list` and `crackling vm inspect`
- [ ] Test: create VM, start it, verify firecracker process running, stop, delete

## Phase 4: Networking

- [ ] Implement TAP device creation via ioctl (`src/network/tap.rs`)
- [ ] Implement IP allocation from /16 pool in /30 blocks (`src/network/ip.rs`)
- [ ] Implement iptables NAT/masquerade rule management (`src/network/nat.rs`)
- [ ] Implement WAN interface detection (`ip route get 8.8.8.8`)
- [ ] Integrate networking into VM start: create TAP, allocate IP, set iptables, configure kernel `ip=` param
- [ ] Integrate cleanup into VM stop/delete: remove iptables rules, delete TAP, release IP
- [ ] Implement state reconciliation: cleanup orphaned TAP devices on startup
- [ ] Test: start VM, verify SSH reachable from host, verify internet from guest

## Phase 5: SSH Exec + File Copy

- [ ] Implement SSH client connection with retry/backoff (`src/ssh/client.rs`)
- [ ] Implement `crackling exec`: remote command execution with streaming I/O (`src/ssh/exec.rs`)
- [ ] Implement `crackling cp`: SCP-style bidirectional file transfer (`src/ssh/copy.rs`)
- [ ] Implement `crackling vm ssh`: convenience wrapper for interactive SSH
- [ ] Test: exec a command, copy a file in both directions

## Phase 6: ZFS Snapshots

- [ ] Implement ZFS snapshot operations: create, rollback, list, destroy (`src/zfs/snapshot.rs`)
- [ ] Implement `crackling snapshot create`
- [ ] Implement `crackling snapshot restore` (enforce VM stopped)
- [ ] Implement `crackling snapshot list`
- [ ] Implement `crackling snapshot delete`
- [ ] Test: snapshot, modify VM, restore, verify original state

## Phase 7: Pause/Resume + YAML Config + Polish

- [ ] Implement `crackling vm pause` via Firecracker `PATCH /vm`
- [ ] Implement `crackling vm resume` via Firecracker `PATCH /vm`
- [ ] Implement YAML config file loading and merge with CLI flags (`src/config/vm.rs`)
- [ ] Add `--config` flag to `vm create`
- [ ] Add `--format json` output to all list/inspect commands
- [ ] Implement cleanup/rollback for partial operations (e.g., TAP created but firecracker failed)
- [ ] Polish error messages across the board
