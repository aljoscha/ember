# macOS Support TODO

> **Note:** "Test:" items are **integration tests** (`tests/*.rs`), not manual testing.
> They should be implemented as `#[test] #[ignore]` functions following the
> same patterns as the existing Linux integration tests.  Use
> `#[cfg(target_os = "macos")]` to restrict them to macOS builds.

## Phase 0: Backend Trait Extraction (Linux-only, no behavior change)

- [x] Define `VmBackend`, `StorageBackend`, `NetworkBackend` traits in `src/backend/mod.rs`
- [x] Move Firecracker code behind `VmBackend` trait (`src/backend/linux/vm.rs`)
- [x] Move ZFS code behind `StorageBackend` trait (`src/backend/linux/storage.rs`)
- [x] Move TAP/iptables/IP-allocation code behind `NetworkBackend` trait (`src/backend/linux/network.rs`)
- [x] Move ext4/loop-mount code behind platform-specific image helpers (`src/backend/linux/image.rs`)
- [x] Update CLI modules to call backend traits instead of direct module calls
- [x] Verify `cargo build` and `cargo test` pass with no behavior change

## Phase 1: Swift Helper (`ember-vz`)

- [x] Create Swift Package Manager project for `ember-vz`
- [x] Implement `ember-vz start`: boot Linux VM with VZLinuxBootLoader, virtio-blk, virtio-net (shared vmnet), virtio-console
- [x] Implement graceful shutdown on SIGTERM (VZVirtualMachine.stop)
- [x] Implement SIGKILL handling (force stop)
- [x] Implement pause (SIGUSR1) and resume (SIGUSR2)
- [x] Implement `--ready-fd` to report guest MAC address when VM is booted
- [x] Implement serial console logging to file
- [x] Test: boot a Linux kernel + minimal rootfs, verify serial output, verify network

## Phase 2: macOS Storage Backend (APFS Clones)

- [x] Implement `StorageBackend` for macOS: init (create directories)
- [x] Implement image volume creation: raw ext4 `.img` file
- [x] Implement VM clone: `cp -c` (APFS CoW clone)
- [x] Implement snapshot create/restore/delete using `cp -c`
- [x] Implement resize: `truncate` + `resize2fs`
- [x] Implement mount/unmount via `hdiutil attach`/`hdiutil detach`
- [x] Implement destroy (remove files)
- [x] Validate APFS volume during `ember init` (`diskutil info` check)
- [x] Catch `cp -c` failures with clear error message (non-APFS, cross-volume)
- [x] Add timing-based sanity check on `cp -c` (warn if clone takes >1s)
- [x] Test: create image, clone for VM, snapshot, restore, resize
- [x] Test: verify `cp -c` clone doesn't reduce free space (df check)
- [ ] Test: verify `cp -c` fails gracefully on non-APFS volume

## Phase 2.5: Storage Efficiency Diagnostics

- [x] Implement `ember debug storage-efficiency` command
- [x] Report logical size (sum of all .img file sizes via stat)
- [x] Report actual disk usage (df / diskutil apfs list)
- [x] Report CoW efficiency ratio
- [x] Test: create base image + multiple clones, verify efficiency report shows savings

## Phase 3: macOS VM Backend (AVF via ember-vz)

- [ ] Implement `VmBackend` for macOS: start (spawn `ember-vz`, wait for ready-fd)
- [ ] Implement stop (SIGTERM + timeout + SIGKILL)
- [ ] Implement pause/resume (SIGUSR1/SIGUSR2)
- [ ] Implement `is_running` (kill(pid, 0))
- [ ] Build or acquire AVF-compatible Linux kernel preset
- [ ] Update `kernel.rs` with macOS-specific preset URL and boot args (`console=hvc0`)
- [ ] Test: full VM lifecycle (start, SSH, stop)

## Phase 4: macOS Networking (vmnet)

- [ ] Implement `NetworkBackend` for macOS: setup (no-op, vmnet handles everything)
- [ ] Implement guest IP discovery from DHCP leases (`/var/db/dhcpd_leases`)
- [ ] Implement ARP-based fallback IP discovery
- [ ] Implement teardown (no-op for shared mode)
- [ ] Implement WAN interface detection (`route get 8.8.8.8` instead of `ip route get`)
- [ ] Test: VM gets DHCP IP, SSH works, internet access from guest

## Phase 5: macOS Image Pipeline

- [ ] Adapt ext4 creation to use `hdiutil attach` instead of `mount -o loop`
- [ ] Verify `skopeo` works on macOS (Homebrew install)
- [ ] Verify SSH key injection works with `hdiutil`-mounted images
- [ ] Adapt `ember image build` for macOS (Docker Desktop / Podman)
- [ ] Test: `ember image pull alpine:latest` end-to-end on macOS

## Phase 6: macOS `ember init` + State Directory

- [ ] Use `~/Library/Application Support/ember/` as default state directory on macOS
- [ ] Skip ZFS pool/dataset creation on macOS
- [ ] Skip root privilege check on macOS
- [ ] Adapt reconciliation: skip TAP/iptables cleanup (not applicable)
- [ ] Test: `ember init` on macOS creates correct directory structure

## Phase 7: Polish + CI

- [ ] Add `cargo build --target aarch64-apple-darwin` to CI
- [ ] Add `cargo build --target x86_64-apple-darwin` to CI
- [ ] Integrate `swift build` for `ember-vz` into build pipeline
- [ ] Create Homebrew formula (bundles both `ember` and `ember-vz`)
- [ ] Update README with macOS installation instructions
- [ ] Update SPEC.md to reference MACOS-SPEC.md
- [ ] End-to-end test on macOS: init → image pull → vm create → ssh → snapshot → restore → delete
