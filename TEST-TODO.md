# Integration Test Unification TODO

> See TEST-SPEC.md for design details.
> Work through tasks one at a time. Implement, verify, check off, commit, stop.

## Phase A: Foundation

- [x] Create `tests/common/linux.rs` — extract all shared Linux helpers from `init.rs`, `image.rs`, `vm.rs`, `snapshot.rs`, `fork.rs`, `ssh.rs`, `resize.rs` (see TEST-SPEC.md "What's in common/linux.rs")
- [x] Create `tests/common/macos.rs` — move macOS helpers out of `common/mod.rs` + extract from `macos_storage.rs` (see TEST-SPEC.md "What's in common/macos.rs")
- [x] Rewrite `tests/common/mod.rs` — cross-platform base with `ember_bin()`, `ember()`, conditional submodule includes, and `TestEnv` struct with `init()`, `with_image()`, `with_vm()` constructors
- [x] Update macOS test files to use new module paths (`common::macos::setup_init` etc.)
- [x] Update Linux test files to use `common::linux::` helpers, delete local copies. One file at a time: `init.rs`, `image.rs`, `snapshot.rs`, `resize.rs`, `fork.rs`, `vm.rs`, `ssh.rs`
- [x] Verify: `cargo build --tests` passes on macOS; run `./run-integration-tests.sh` for a few suites

## Phase B: Unify Pure CLI Tests (no hypervisor needed)

- [x] Unify `init.rs` + `macos_init.rs` → single `init.rs`. Delete `macos_init.rs`
- [x] Unify `image.rs` + `macos_image.rs` → single `image.rs`. Delete `macos_image.rs`
- [x] Unify `snapshot.rs` + snapshot tests from `macos_storage.rs` → single `snapshot.rs`
- [x] Unify `resize.rs` + resize tests from `macos_storage.rs` → single `resize.rs`
- [x] Unify `fork.rs` — remove `#![cfg(target_os = "linux")]`, use `TestEnv`
- [x] Verify: `cargo build --tests` on macOS; `./run-integration-tests.sh` for unified suites

## Phase C: Unify Running-VM Tests

- [x] Add `TestEnv::with_running_vm()` constructor (platform-specific prerequisites, returns Option)
- [x] Unify `vm.rs` + `macos_vm.rs` + `macos_network.rs` → single `vm.rs`. Delete `macos_vm.rs`, `macos_network.rs`
- [ ] Unify `ssh.rs` — remove `#![cfg(target_os = "linux")]`, use `TestEnv`
- [ ] Verify: `cargo build --tests` on macOS; `./run-integration-tests.sh vm ssh`

## Phase D: Cleanup

- [ ] Slim `macos_storage.rs` — remove snapshot/resize tests that moved to unified files, keep APFS-specific tests only
- [ ] Update `run-integration-tests.sh` if needed (should auto-detect, but verify)
- [ ] Final verify: full `./run-integration-tests.sh` on macOS, confirm test count matches expectations
