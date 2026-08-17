# Plan: Unify Integration Tests Between Linux and macOS

## Context

The integration test suite has 13 files (7 Linux, 6 macOS) that grew organically during platform-specific development. Two problems:

1. **~500 lines of duplicated helpers** across Linux test files (`ember_bin()`, `ember()`, `test_pool()`, `create_loop_device()`, `PoolCleanup`, etc. — repeated in all 7 files). macOS tests already share helpers via `tests/common/mod.rs`.

2. **Duplicated test logic**: The CLI interface is identical on both platforms. Tests for image, snapshot, resize, fork, ssh, and VM lifecycle exercise the same CLI commands with the same assertions. The only difference is setup (Linux: ZFS pool + loopback device, macOS: temp directory).

The macOS tests were written during incremental development and bypass the CLI in some places (e.g., `macos_vm.rs` tests ember-vz directly, `macos_storage.rs` manually creates VMs instead of using `ember vm create`). But the full CLI pipeline is implemented and working on macOS — all backend traits are fully implemented.

**Goal**: One test file per feature that works on both platforms, with platform differences isolated to setup and optional platform-specific verification.

## `TestEnv` — The Core Abstraction

A `TestEnv` struct in `tests/common/mod.rs` encapsulates platform-specific setup:

```rust
pub struct TestEnv {
    pub state_dir: PathBuf,
    #[cfg(target_os = "linux")]
    pub pool: String,
    _cleanup: Box<dyn std::any::Any>,  // PoolCleanup on Linux, nothing on macOS
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    pub fn state(&self) -> &str;

    /// Just `ember init`.
    pub fn init(test_name: &str) -> Self;

    /// `ember init` + `ember image pull alpine:latest`.
    pub fn with_image(test_name: &str) -> Self;

    /// `ember init` + image pull + `ember vm create --no-start`.
    pub fn with_vm(test_name: &str, vm_name: &str) -> Self;

    /// Full running VM with SSH access. Returns None if prerequisites missing.
    /// Linux: needs Firecracker + /dev/kvm + docker (ubuntu-slim)
    /// macOS: needs ember-vz + AVF kernel
    pub fn with_running_vm(test_name: &str, vm_name: &str) -> Option<Self>;
}
```

Platform-specific setup inside `TestEnv`:
- **Linux**: creates loopback device → ZFS pool → `ember init --pool X --device Y`
- **macOS**: `ember init` (no extra args, temp directory suffices)

Both use `ember image pull`, `ember vm create --no-start`, etc. through the CLI — identical commands.

## File Structure After Unification

```
tests/
  common/
    mod.rs           # TestEnv, ember_bin(), ember(), cross-platform helpers
    linux.rs         # ZFS/loopback/PoolCleanup, firecracker_available(), ensure_kernel()
    macos.rs         # APFS helpers, ember-vz resolution, e2fsprogs lookup
  init.rs            # UNIFIED — shared + platform-specific init tests
  image.rs           # UNIFIED — pull, list, delete, idempotent re-pull
  vm.rs              # UNIFIED — create, inspect, start, stop, delete, pause/resume
  snapshot.rs        # UNIFIED — create, list, restore, delete, error cases
  resize.rs          # UNIFIED — grow, shrink-fails, multiple-grows, metadata check
  fork.rs            # UNIFIED — basic, overrides, delete-cleanup, error cases
  ssh.rs             # UNIFIED — exec, cp, exec-on-stopped-fails
  macos_storage.rs   # macOS-only: APFS clone efficiency, HFS+ fallback, storage usage
  macos_ember_vz.rs  # macOS-only: low-level ember-vz component tests (optional, for debugging)
```

Files deleted: `macos_init.rs`, `macos_image.rs`, `macos_vm.rs`, `macos_network.rs`
(Their test coverage is subsumed by the unified files.)

## Step-by-Step Migration

### Step 1: Create `common/` submodule structure

**Create `tests/common/linux.rs`** with all helpers extracted from current Linux test files:
- `test_pool()`, `create_loop_device()`, `create_loop_device_sized()`, `detach_loop_device()`, `destroy_pool()`, `PoolCleanup`
- `assert_pool_exists()`, `assert_dataset_exists()`, `assert_snapshot_exists()`, `assert_snapshot_absent()`, `assert_zvol_exists()`, `assert_zvol_absent()`
- `wait_for_zvol_device()`, `with_mounted_zvol()`, `get_zvol_size_bytes()`
- `firecracker_available()`, `docker_available()`
- Linux `ensure_kernel()` (downloads Firecracker kernel)
- `ssh_private_key_path()`, `ssh_exec()`, `wait_for_ssh()`

**Create `tests/common/macos.rs`** from current `common/mod.rs` + `macos_storage.rs` helpers:
- `find_e2fsprogs_tool()`, `create_test_rootfs()`
- macOS `ensure_kernel()` (local build, no download)
- `ember_vz_bin()`, `spawn_ember_vz()`, `read_mac_from_pipe()`, `wait_for_exit()`

**Rewrite `tests/common/mod.rs`** as cross-platform base:
- `ember_bin()`, `ember()` (already cross-platform)
- `TestEnv` struct + all constructors
- `#[cfg(target_os = "linux")] pub mod linux;`
- `#[cfg(target_os = "macos")] pub mod macos;`

**Files**: create `tests/common/linux.rs`, `tests/common/macos.rs`; rewrite `tests/common/mod.rs`

### Step 2: Unify `init.rs` + `macos_init.rs`

Remove `#![cfg(target_os)]` from both. Merge into one `init.rs`.

**Shared tests** (identical on both platforms):
- `init_creates_directory_structure` — checks `vms/`, `kernels/`, `images/`, `network/` dirs exist
- `init_writes_config_json` — checks config.json is valid JSON with expected fields
- `init_is_idempotent` — running init twice succeeds, dirs still exist

**Platform-specific tests** (keep as `#[cfg]` within same file):
- Linux: `init_creates_pool_and_datasets` — ZFS pool/dataset verification
- Linux: `init_fails_without_device` — `--device` flag requirement
- Linux: `init_custom_dataset_name` — `--dataset` flag
- macOS: `init_works_without_root` — checks `euid != 0`

**Delete**: `tests/macos_init.rs`

### Step 3: Unify `image.rs` + `macos_image.rs`

Merge into one `image.rs` using `TestEnv::with_image()`.

**Shared tests** (identical CLI + assertions):
- `pull_creates_image` — pull alpine, verify success message
- `list_shows_pulled_image` — table + JSON output
- `delete_removes_image` — registry entry + file gone, list shows "No images found"
- `pull_same_image_twice_is_idempotent` — "already exists" message

**Platform-specific verification** (within shared tests via `#[cfg]` blocks):
- Linux: check ZFS zvol exists after pull
- macOS: check `.img` file exists in `images/data/`

**Delete**: `tests/macos_image.rs`

### Step 4: Unify `snapshot.rs` + snapshot tests from `macos_storage.rs`

Merge into one `snapshot.rs` using `TestEnv::with_vm()`.

**Shared tests**:
- `snapshot_create_list_delete` — full lifecycle with table + JSON list
- `snapshot_create_duplicate_fails`
- `snapshot_create_base_name_rejected`
- `snapshot_restore_nonexistent_fails`
- `snapshot_delete_nonexistent_fails`
- `snapshot_list_empty`

**Platform-specific tests** (in same file, `#[cfg]`):
- Linux: `snapshot_restore_reverts_changes` — mounts zvol, writes data, restores, verifies revert
- Linux: `snapshot_delete_base_rejected`
- Linux: `snapshot_on_nonexistent_vm_fails`

Remove snapshot tests from `macos_storage.rs` (they move to unified `snapshot.rs`).

### Step 5: Unify `resize.rs` + resize tests from `macos_storage.rs`

Merge into one `resize.rs` using `TestEnv::with_vm()`.

**Shared tests**:
- `resize_shrink_fails`
- `resize_multiple_grows` — with metadata verification via `ember vm inspect`
- `resize_nonexistent_vm_fails`

**Platform-specific tests** (in same file, `#[cfg]`):
- Linux: `resize_grows_disk` — mounts zvol, checks df
- macOS: `resize_grows_disk` — uses dumpe2fs to check ext4 block count

Remove resize tests from `macos_storage.rs`.

### Step 6: Unify `fork.rs`

Remove `#![cfg(target_os = "linux")]`. Use `TestEnv::with_vm()`.

**Shared tests** (all use `--no-start`, no hypervisor needed):
- `fork_with_overrides` — checks cpus/memory overrides in metadata
- `fork_nonexistent_source_fails`
- `fork_duplicate_name_fails`
- `fork_shrink_disk_fails`
- `fork_basic` — fork, inspect forked VM's metadata (image, status, forked_from)

**Platform-specific tests** (in same file, `#[cfg]`):
- Linux: `fork_delete_cleans_up_snapshot` — checks ZFS zvol/snapshot cleanup
- Linux: `fork_delete_source_with_dependent_snapshot`
- Linux: `fork_preserves_disk_data` — mounts zvol, writes data, verifies in fork

### Step 7: Unify `vm.rs` + `macos_vm.rs`

Remove `#![cfg(target_os)]` from `vm.rs`. Rewrite to use `TestEnv`.

**Shared tests** (use `ember` CLI, not ember-vz directly):
- `vm_create_and_inspect` — `--no-start`, check metadata
- `vm_list` — table + JSON output
- `vm_delete` — verify VM is gone
- `vm_start_stop` — needs `TestEnv::with_running_vm()`, checks status transitions
- `vm_pause_resume` — same
- `vm_force_stop`

**Platform-specific tests** (`#[cfg]`):
- Linux: networking-specific (TAP device, IP allocation via iptables)
- macOS: vmnet-specific (static IP boot args verification)

Keep `macos_ember_vz.rs` as an optional low-level component test (useful for debugging if CLI-level tests fail).

**Delete**: `tests/macos_vm.rs`, `tests/macos_network.rs`

### Step 8: Unify `ssh.rs`

Remove `#![cfg(target_os = "linux")]`.

**Shared tests**:
- `exec_on_stopped_vm_fails` — uses `TestEnv::with_vm()` (--no-start), no hypervisor needed
- `exec_command_returns_stdout` — uses `TestEnv::with_running_vm()`, skip if prerequisites missing
- `cp_upload_and_download` — same

### Step 9: Slim down `macos_storage.rs`

After steps 4-5 extracted snapshot/resize tests, `macos_storage.rs` retains only APFS-specific tests:
- `apfs_clone_does_not_reduce_free_space`
- `storage_usage_reports_images_and_vms`
- `vm_delete_removes_storage`
- `cp_c_fails_gracefully_on_non_apfs`

## Execution Order

**Phase A — Foundation (steps 1)**: Create common submodules + TestEnv. No test files change yet.

**Phase B — Easy wins (steps 2-5)**: Unify init, image, snapshot, resize. These all use `--no-start` and don't need a running hypervisor. Low risk — if something fails on macOS it's a real bug to fix.

**Phase C — Bigger merges (steps 6-8)**: Unify fork, vm, ssh. Fork is easy (all --no-start). VM and SSH need the `with_running_vm()` path which requires platform-specific hypervisor setup.

**Phase D — Cleanup (step 9)**: Slim down macos_storage.rs.

## Verification

After each step:
- `cargo build --tests` on current platform
- `./run-integration-tests.sh <modified_test>` for each modified file
- Verify test count: `cargo test --test <name> --ignored --list 2>/dev/null | grep -c ": test$"` shouldn't decrease
- On the other platform (if accessible): same checks

## Critical Files

- `tests/common/mod.rs` — restructure into cross-platform base with TestEnv
- `tests/common/linux.rs` — new, extracted from 7 Linux test files
- `tests/common/macos.rs` — new, extracted from current common/mod.rs + macos_storage.rs helpers
- All 13 test files in `tests/` — modified or deleted
- `run-integration-tests.sh` — may need minor updates for renamed/deleted test files
