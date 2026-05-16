# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is ember?

A lightweight CLI for managing microVMs with copy-on-write storage. CLI-only — no daemon, no REST API.

- **Linux**: Firecracker (KVM) + one of:
  - ZFS zvols (default; see `docs/SPEC.md`).
  - dm-thin (kernel-builtin device-mapper thin provisioning; see `docs/DM-THIN-SPEC.md`).
  Backend is selected at `ember init --storage <zfs|dm-thin>` and persisted on `GlobalConfig`.
- **macOS**: Apple Virtualization Framework + APFS clones. See `docs/MACOS-SPEC.md` for the design.

## Build Commands

```bash
# Build
cargo build

# Build and run
cargo run -- --help

# Run tests
cargo test

# Format
cargo fmt

# Check without building
cargo check

# Lint
cargo clippy
```

## Testing

```bash
# Unit tests
cargo test

# Manual testing (requires root, firecracker, and a backend)

# ZFS backend
sudo ./target/debug/ember init --pool testpool --device /dev/loop0
sudo ./target/debug/ember image pull alpine:latest
sudo ./target/debug/ember vm create testvm --image alpine:latest

# dm-thin backend (no kernel module; in-tree)
sudo ./target/debug/ember init \
    --storage dm-thin \
    --storage-path /var/lib/ember/dm-thin \
    --size 50G
sudo ./target/debug/ember image pull alpine:latest
sudo ./target/debug/ember vm create testvm --image alpine:latest

# Tear down a backend
sudo ./target/debug/ember deinit --purge

# Grow the dm-thin data device
sudo ./target/debug/ember storage grow --size 100G

# Integration tests for dm-thin (root + dm-thin module + thin-provisioning-tools)
sudo cargo test --test dm_thin -- --ignored --test-threads=1
```

## Coding Style & Conventions

- Prefer explicit error handling. Use `?` for propagation, not `.unwrap()`.
- Shell out to platform CLI tools — no fragile C library bindings. Linux: `zfs`/`zpool`/`iptables`. macOS: `hdiutil`/`diskutil`/`cp -c`/`ember-vz`.
- Value clear interfaces, boundaries, and abstractions; avoid leaks between them. Subsystems own their own formats — dm-thin owns its pool/volume names, networking owns its TAP prefix and iptables comment, and so on. Shared types like `GlobalConfig` expose generic identity (e.g. `instance_namespace()`) and stay free of subsystem trivia. If you find yourself reaching across a boundary to format a name, match a string, or branch on another subsystem's mode, that's the cue to move the logic to the side that owns the concept.

## Architecture

See specs in the docs/ folder for details, when needed.

Basic architecture choices:

- Platform-specific code lives behind backend traits (`VmBackend`, `StorageBackend`, `NetworkBackend`).
- `Vm` and `Network` are picked at compile time via `#[cfg(target_os)]`. `Storage` is a runtime trait object (`Arc<dyn StorageBackend>`) so the concrete backend can be selected from `GlobalConfig.storage_backend` without a rebuild.
- Shell out to platform tools: `ember-vz` (Swift helper for AVF), `hdiutil`, `diskutil`, `cp -c`, Homebrew `e2fsprogs` on macOS; `zfs`/`zpool`/`iptables`/`dmsetup`/`losetup`/`thin-provisioning-tools` on Linux.

## Version Control

We use jujutsu (jj) for version control; prefer jj over git when possible.
The main branch/bookmark is `main`.

- Create individual jj changes with good descriptions; one logical change per commit.
- Prefix change description titles with the subsystem, e.g. `cli: implement CLI parsing` or `zfs: add pool operations`.
- Verify `cargo build` passes before finalizing a change.
- After `jj describe`, normally run `jj new` to create a fresh change for unrelated or follow-up work.

### jj Operations

- When fixing compilation across multiple changes after a rebase, work oldest-to-newest, one change at a time. Run `cargo build` and verify it passes before moving to the next change.
- Prefer manual file-level reverts over `jj backout` when the change touches files modified in descendant changes.
- When squashing, always verify the target change is correct before executing.
- Use `jj undo` immediately when an operation creates cascading conflicts, rather than trying to fix the mess.
- Never squash or reorder changes without asking first.
