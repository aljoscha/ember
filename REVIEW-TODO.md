# Code Review TODO

Issues from the macOS-support branch code review, ordered by priority.

## Critical

- [x] **ember-vz: `stop()` uses forceful kill, not graceful ACPI shutdown**
  `ember-vz/Sources/EmberVZ/Start.swift:165` — `VZVirtualMachine.stop()` is an immediate
  forceful stop. Should use `VZVirtualMachine.requestStop()` to send an ACPI power button
  event so the guest can cleanly unmount filesystems.

- [x] **ember-vz: `pause`/`resume` completion handler signatures wrong**
  `ember-vz/Sources/EmberVZ/Start.swift:187, 209` — Uses `Result<Void, Error>` but AVF's
  `pause`/`resume` completion handlers take `(Error?) -> Void`. Will fail under strict
  Swift 6 type checking.
  **Not an issue**: AVF on this SDK uses `Result<Void, Error>`, existing code is correct.

- [x] **`pause`/`resume` CLI commands broken on macOS**
  `src/cli/vm.rs:830-839, 872-881` — Both check `metadata.api_socket.exists()` (Firecracker
  socket) before dispatching. On macOS, ember-vz uses signals and the socket never exists.
  Gate the socket check with `#[cfg(target_os = "linux")]`.

## High

- [x] **Non-atomic `restore_snapshot` — data loss risk**
  `src/backend/macos/storage.rs:227-234` — Rootfs is deleted before cloning the snapshot.
  If the clone fails, VM has no disk. Clone to a temp file, then atomically rename.

- [x] **2,590 Swift build artifacts committed**
  `ember-vz/.build/` — Despite `.gitignore`, the entire build dir was committed. Remove
  from version control history.
  **Already fixed**: `.build/` is gitignored and not tracked.

- [ ] **ARP IP discovery bug — early return on non-matching lines**
  `src/backend/macos/network.rs:235` — `?` in `find_ip_in_arp_output` causes early return
  on any line without `" at "`. Use `continue` on parse failure instead.

## Medium

- [ ] **File descriptor leak on spawn failure**
  `src/backend/macos/vm.rs:100-159` — `read_raw` from `into_raw_fd()` is never closed if
  `cmd.spawn()` fails. Wrap in `File`/`OwnedFd` immediately.

- [ ] **TOCTOU race in `stop()`/`force_stop()`**
  `src/backend/macos/vm.rs:214, 242` — `is_running` check then `kill()`. Handle `ESRCH`
  from `kill()` directly instead of pre-checking.

- [ ] **`info` command shows Linux-specific output on macOS**
  `src/cli/info.rs:12, 27-28` — Prints "ZFS pool:"/"Dataset:" unconditionally and
  references `--pool`/`--device` in error hint. Use `#[cfg]` for platform output.

- [ ] **`image inspect` prints "ZFS zvol:" on macOS**
  `src/cli/image.rs:330` — Label should be platform-conditional ("Disk image:" on macOS).

- [ ] **`udevadm settle` called on macOS**
  `src/cli/vm.rs:983` — Linux-only tool in shared `force_delete_vm`. Gate with
  `#[cfg(target_os = "linux")]`.

- [ ] **`e2fsck` exit code handling is wrong**
  `src/backend/macos/storage.rs:364` — Checks `!= Some(1)` but e2fsck uses a bitmask.
  Should check error bits (`code & 0b1100 != 0`) or use `>= 2` like the Linux backend.

- [ ] **`InitArgs` exposes `--pool`/`--device`/`--dataset` on macOS**
  `src/cli/init.rs:12-32` — Linux-specific flags visible to macOS users. Hide or
  document as Linux-only.

- [ ] **No input validation on `ember-vz` CPU/memory values**
  `ember-vz/Sources/EmberVZ/Start.swift:81-82` — Validate against AVF
  `minimumAllowed*`/`maximumAllowed*` before configuring.

- [ ] **VM errors use `Error::Network` variant**
  `src/backend/macos/vm.rs:212, 240, 264, 283` — Add an `Error::Vm` variant for
  hypervisor lifecycle errors.

## Low

- [ ] **Linux-specific field names in shared types**
  `src/state/vm.rs` — `zvol_path` (should be `disk_path`), `api_socket`
  (Firecracker-specific), `NetworkInfo.tap_device`/`netmask` (empty on macOS).
  Rename with `#[serde(alias)]` for backward compat.

- [ ] **macOS reconciliation runs unconditionally**
  `src/main.rs:95-96` — No `needs_reconcile` guard. Even `ember version` triggers
  a state dir scan.

- [ ] **Docstrings reference Linux-only concepts**
  `src/cli/vm.rs:809, 851, 640` — "Firecracker PATCH /vm API", "ZFS clone @base".
  Make generic or add platform notes.

- [ ] **Duplicate `format_bytes` functions**
  `src/cli/debug.rs:168` vs `src/cli/snapshot.rs:186` — One SI, one binary.
  Consolidate into a shared utility.

- [ ] **Massive test helper duplication**
  `tests/macos_*.rs` — Helpers copy-pasted across 6 files. Extract to
  `tests/common.rs`.

- [ ] **No `compile_error!` for unsupported platforms**
  `src/backend/mod.rs` — Add fallback for non-Linux/macOS targets.

- [ ] **`discover_guest_ip` returns error on Linux**
  `src/backend/linux/network.rs:91-97` — Trait method always errors. Consider a
  default impl that returns an error, or document the platform-specific calling convention.

- [ ] **`ember-vz` `--network` option value is ignored**
  `Start.swift:37, 97` — Always creates NAT regardless of value. Validate or remove.

- [ ] **Boot args default mismatch**
  `Start.swift:34` — Missing `ip=dhcp` vs Rust side. Unused in practice but confusing
  for manual ember-vz invocations.

- [ ] **`inject_inittab` writes Firecracker-specific `ttyS0` console**
  `src/image/inject.rs:239-247` — AVF uses `hvc0`. May configure wrong console.

- [ ] **Cargo.toml description is outdated**
  Says "Lightweight Firecracker VM manager with ZFS-backed storage" — no longer
  accurate with macOS/AVF support.

- [ ] **`debugfs` error detection is fragile**
  `src/backend/macos/storage.rs:579-587` — String matching on stderr messages is
  version-dependent. Consider a more robust check.

- [ ] **No shrink guard in storage backend**
  `src/backend/macos/storage.rs:330` — Relies on CLI layer to prevent shrink.
  Add a defensive size check.

- [ ] **Incomplete OCI opaque whiteout handling**
  `src/image/pull.rs:371-374` — `.wh..wh..opq` marker is removed but previous-layer
  entries are not cleared.
