# Integration Test Unification Spec

This document specifies how ember's integration tests should be structured to provide unified, cross-platform CLI testing for both Linux and macOS.

## Current State

The integration test suite has 13 files split by platform:

| Linux (7 files) | macOS (6 files) |
|-----------------|-----------------|
| `init.rs` | `macos_init.rs` |
| `image.rs` | `macos_image.rs` |
| `vm.rs` | `macos_vm.rs` |
| `snapshot.rs` | `macos_storage.rs` |
| `resize.rs` | `macos_network.rs` |
| `fork.rs` | `macos_ember_vz.rs` |
| `ssh.rs` | |

### Problems

1. **~500 lines of duplicated helpers** across Linux test files. Each of `init.rs`, `image.rs`, `vm.rs`, `snapshot.rs`, `fork.rs`, `ssh.rs`, `resize.rs` independently defines `ember_bin()`, `ember()`, `test_pool()`, `create_loop_device()`, `detach_loop_device()`, `destroy_pool()`, `PoolCleanup`, and setup functions. macOS tests already share helpers via `tests/common/mod.rs`, but that module is macOS-only.

2. **Duplicated test logic**. The CLI interface is identical on both platforms. Tests for image, snapshot, resize, fork, SSH, and VM lifecycle exercise the same commands with the same assertions. The only difference is setup (Linux: ZFS pool + loopback device, macOS: temp directory).

3. **macOS tests bypass the CLI** in some places. `macos_vm.rs` tests `ember-vz` directly (not through `ember`), `macos_storage.rs` manually creates VMs instead of using `ember vm create`, and `macos_network.rs` tests ember-vz directly. These were written during incremental development. The full CLI pipeline is now implemented on both platforms.

## Design Principles

- **Same CLI, same tests.** One test file per feature, compiling and running on both platforms. The `ember` CLI presents the same interface on Linux and macOS — tests should reflect that.
- **Platform differences live in setup, not assertions.** A `TestEnv` abstraction handles platform-specific setup (ZFS pool vs. temp directory). Test assertions verify CLI output and behavior, which is identical across platforms.
- **Black-box CLI testing.** All integration tests shell out to the compiled `ember` binary via `Command::new()`. No internal function calls. (Unit tests in `src/` cover internals.)
- **`#[cfg]` for platform-specific verification.** Some tests need to verify platform internals (e.g., ZFS datasets exist, APFS clone is zero-copy). These are `#[cfg(target_os)]` blocks within shared test functions, not separate files.
- **Pure CLI tests first.** Tests using `--no-start` (no hypervisor) are the easiest to unify and should be migrated first. Tests requiring running VMs (SSH, start/stop lifecycle) come second.

## `TestEnv` Abstraction

`TestEnv` encapsulates platform-specific test setup behind a uniform interface. It lives in `tests/common/mod.rs`.

```rust
pub struct TestEnv {
    pub state_dir: PathBuf,
    #[cfg(target_os = "linux")]
    pub pool: String,
    _cleanup: Box<dyn std::any::Any>,
    _tmp: tempfile::TempDir,
}
```

### Constructors

Each constructor calls the `ember` CLI to set up state, returning a `TestEnv` ready for the test to use.

| Constructor | What it does | Hypervisor needed? |
|-------------|-------------|-------------------|
| `TestEnv::init(name)` | `ember init` | No |
| `TestEnv::with_image(name)` | init + `ember image pull alpine:latest` | No |
| `TestEnv::with_vm(name, vm)` | with_image + `ember vm create --no-start` | No |
| `TestEnv::with_running_vm(name, vm)` | with_vm + `ember vm start` + wait for SSH | Yes |

### Platform-Specific Setup

**Linux** (`TestEnv::init`):
1. Create loopback file with `truncate`
2. Attach to loop device with `losetup`
3. `ember init --pool <unique_name> --device <loop_dev>`
4. Store `PoolCleanup` in `_cleanup` (drops ZFS pool + detaches loop device)

**macOS** (`TestEnv::init`):
1. `ember init --state-dir <tmpdir>/state`
2. No special cleanup needed (tempdir handles it)

### Helper Methods

```rust
impl TestEnv {
    /// State directory path as &str (for CLI args).
    pub fn state(&self) -> &str;

    /// Run ember with --state-dir prepended.
    pub fn ember(&self, args: &[&str]) -> Output;
}
```

### Running VM Setup

`TestEnv::with_running_vm()` returns `Option<TestEnv>` — `None` if prerequisites are missing.

**Linux prerequisites**: `firecracker` in PATH, `/dev/kvm`, `docker` (to build ubuntu-slim image), kernel (auto-downloaded or `EMBER_TEST_KERNEL`), SSH key.

**macOS prerequisites**: `ember-vz` built, AVF-compatible kernel (see `ensure_kernel()`), `e2fsprogs` (Homebrew).

## File Structure

```
tests/
  common/
    mod.rs           # TestEnv + cross-platform helpers (ember_bin, ember)
    linux.rs         # ZFS/loopback helpers, PoolCleanup, firecracker/docker checks,
                     # kernel download, SSH helpers, ZFS assertion functions
    macos.rs         # e2fsprogs lookup, ember-vz resolution, ensure_kernel (macOS),
                     # create_test_rootfs, spawn_ember_vz, pipe helpers
  init.rs            # UNIFIED
  image.rs           # UNIFIED
  vm.rs              # UNIFIED
  snapshot.rs        # UNIFIED
  resize.rs          # UNIFIED
  fork.rs            # UNIFIED
  ssh.rs             # UNIFIED
  macos_storage.rs   # macOS-only: APFS clone efficiency, HFS+ fallback
  macos_ember_vz.rs  # macOS-only: low-level ember-vz component tests
```

### What's in `common/linux.rs`

Everything currently duplicated across the 7 Linux test files:

- `test_pool(name) -> String` — unique pool name per test
- `create_loop_device(dir) -> (String, PathBuf)` — 512M default
- `create_loop_device_sized(dir, size) -> (String, PathBuf)`
- `detach_loop_device(dev)`
- `destroy_pool(pool)`
- `PoolCleanup` struct + `Drop`
- `assert_pool_exists(pool)`, `assert_dataset_exists(dataset)`, `assert_dataset_absent(dataset)`
- `assert_snapshot_exists(snapshot)`, `assert_snapshot_absent(snapshot)`
- `assert_zvol_exists(zvol)`, `assert_zvol_absent(zvol)`
- `wait_for_zvol_device(path) -> bool`
- `with_mounted_zvol(device, closure) -> T`
- `get_zvol_size_bytes(zvol) -> u64`
- `firecracker_available() -> bool`, `docker_available() -> bool`
- `ensure_kernel() -> Option<PathBuf>` (downloads Firecracker kernel)
- `ssh_private_key_path() -> Option<PathBuf>`
- `ssh_exec(ip, key, cmd) -> Result<String, String>`
- `wait_for_ssh(ip, key) -> bool`

### What's in `common/macos.rs`

Extracted from current `common/mod.rs` + `macos_storage.rs`:

- `find_e2fsprogs_tool(name) -> String`
- `create_test_rootfs(dir, size_mb) -> PathBuf`
- `ember_vz_bin() -> Option<PathBuf>`
- `ensure_kernel() -> Option<PathBuf>` (local build, no download)
- `spawn_ember_vz(...)` — pipe-based process spawning
- `read_mac_from_pipe(file, timeout) -> Option<String>`
- `wait_for_exit(child, timeout) -> ExitStatus`

## Per-File Unification Details

### `init.rs` (merge `init.rs` + `macos_init.rs`)

**Shared tests** (identical on both platforms):
- `init_creates_directory_structure` — checks `vms/`, `kernels/`, `images/`, `network/` exist
- `init_writes_config_json` — valid JSON with expected fields
- `init_is_idempotent` — running init twice succeeds

**Linux-only** (`#[cfg(target_os = "linux")]`):
- `init_creates_pool_and_datasets` — ZFS pool + dataset verification
- `init_fails_without_device` — `--device` flag requirement when pool missing
- `init_custom_dataset_name` — `--dataset mydata`

**macOS-only** (`#[cfg(target_os = "macos")]`):
- `init_works_without_root` — verifies `euid != 0`

**Deletes**: `tests/macos_init.rs`

### `image.rs` (merge `image.rs` + `macos_image.rs`)

Uses `TestEnv::with_image()`.

**Shared tests**:
- `pull_creates_image` — success message, platform-specific `#[cfg]` block to verify storage (ZFS zvol vs .img file)
- `list_shows_pulled_image` — table + JSON output assertions
- `delete_removes_image` — "No images found" after delete
- `pull_same_image_twice_is_idempotent` — "already exists" message

**Deletes**: `tests/macos_image.rs`

### `snapshot.rs` (merge `snapshot.rs` + snapshot tests from `macos_storage.rs`)

Uses `TestEnv::with_vm()`.

**Shared tests**:
- `snapshot_create_list_delete` — full lifecycle with table + JSON verification
- `snapshot_create_duplicate_fails`
- `snapshot_create_base_name_rejected`
- `snapshot_restore_nonexistent_fails`
- `snapshot_delete_nonexistent_fails`
- `snapshot_list_empty`

**Linux-only** (`#[cfg]`):
- `snapshot_restore_reverts_changes` — mounts zvol, writes data, restores, verifies revert
- `snapshot_delete_base_rejected`
- `snapshot_on_nonexistent_vm_fails`

### `resize.rs` (merge `resize.rs` + resize tests from `macos_storage.rs`)

Uses `TestEnv::with_vm()`.

**Shared tests**:
- `resize_shrink_fails`
- `resize_multiple_grows` — with metadata verification via `ember vm inspect --format json`
- `resize_nonexistent_vm_fails`

**Platform-specific** (`#[cfg]`):
- Linux: `resize_grows_disk` — mounts zvol, checks `df`
- macOS: `resize_grows_disk` — uses `dumpe2fs` to check ext4 block count

### `fork.rs` (unify — remove `#![cfg(target_os = "linux")]`)

Uses `TestEnv::with_vm()`. All fork tests use `--no-start`.

**Shared tests**:
- `fork_basic` — fork, inspect metadata (image, status, forked_from)
- `fork_with_overrides` — cpus/memory overrides
- `fork_nonexistent_source_fails`
- `fork_duplicate_name_fails`
- `fork_shrink_disk_fails`

**Linux-only** (`#[cfg]`):
- `fork_delete_cleans_up_snapshot` — ZFS zvol/snapshot verification
- `fork_delete_source_with_dependent_snapshot`
- `fork_preserves_disk_data` — mounts zvol, writes data, verifies in fork

### `vm.rs` (merge `vm.rs` + `macos_vm.rs` + `macos_network.rs`)

Uses `TestEnv::with_vm()` for create/inspect/delete, `TestEnv::with_running_vm()` for start/stop/pause/resume.

**Shared tests**:
- `vm_create_and_inspect` — `--no-start`, check JSON metadata
- `vm_list` — table + JSON output
- `vm_delete` — verify VM is gone
- `vm_start_stop` — `with_running_vm()`, check status transitions (skip if prerequisites missing)
- `vm_pause_resume` — same
- `vm_force_stop` — same

**Platform-specific** (`#[cfg]`):
- Linux: TAP device verification, IP allocation via iptables
- macOS: static IP boot args verification, vmnet checks

**Deletes**: `tests/macos_vm.rs`, `tests/macos_network.rs`

### `ssh.rs` (unify — remove `#![cfg(target_os = "linux")]`)

**Shared tests**:
- `exec_on_stopped_vm_fails` — `TestEnv::with_vm()`, no hypervisor needed
- `exec_command_returns_stdout` — `TestEnv::with_running_vm()`, skip if prerequisites missing
- `cp_upload_and_download` — same

### `macos_storage.rs` (stays macOS-only, slimmed down)

Snapshot and resize tests move to unified files. What remains:
- `apfs_clone_does_not_reduce_free_space`
- `storage_efficiency_shows_savings`
- `vm_delete_removes_storage`
- `cp_c_fails_gracefully_on_non_apfs`

### `macos_ember_vz.rs` (stays macOS-only)

Low-level component test for the `ember-vz` Swift helper. Useful for debugging when CLI-level tests fail. Tests ember-vz directly (spawn, ready-fd pipe, signals), not through the ember CLI.

## Test Runner

`run-integration-tests.sh` needs minor updates:
- Unified files no longer have `#![cfg(target_os)]` at the crate level — they compile on both platforms and use function-level `#[cfg]` instead
- The runner continues to use `sudo` on Linux, normal user on macOS
- Test files that were deleted should be removed from any hardcoded lists (there are none — the runner globs `tests/*.rs`)

## Conventions

- Every integration test is `#[test] #[ignore]` — run via `./run-integration-tests.sh`, not `cargo test`
- Each test file declares `#[allow(dead_code)] mod common;`
- RAII cleanup: use `TestEnv` (which holds cleanup guards via `_cleanup: Box<dyn Any>`)
- Unique names per test: `TestEnv` uses `format!("embertest_{name}_{}", std::process::id())` for pool/VM names to avoid collisions
- Skip gracefully when prerequisites are missing (return early, don't panic)
