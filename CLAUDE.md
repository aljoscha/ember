# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is ember?

A lightweight CLI for managing microVMs with copy-on-write storage. CLI-only — no daemon, no REST API.

- **Linux**: Firecracker (KVM) + ZFS zvols. See SPEC.md for the full design, TODO.md for the task list.
- **macOS**: Apple Virtualization Framework + APFS clones. See MACOS-SPEC.md for the design, MACOS-TODO.md for the task list.

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

# Manual testing (requires root, ZFS, and firecracker installed)
sudo ./target/debug/ember init --pool testpool --device /dev/loop0
sudo ./target/debug/ember image pull alpine:latest
sudo ./target/debug/ember vm create testvm --image alpine:latest
```

## Coding Style & Conventions

- Prefer explicit error handling. Use `?` for propagation, not `.unwrap()`.
- Shell out to platform CLI tools — no fragile C library bindings. Linux: `zfs`/`zpool`/`iptables`. macOS: `hdiutil`/`diskutil`/`cp -c`/`ember-vz`.

## Architecture

See specs in the docs/ folder for details, when needed.

Basic architecture choices:

- Platform-specific code lives behind backend traits (`VmBackend`, `StorageBackend`, `NetworkBackend`) with `#[cfg(target_os)]` compile-time selection.
- Shell out to platform tools: `ember-vz` (Swift helper for AVF), `hdiutil`, `diskutil`, `cp -c`, Homebrew `e2fsprogs`.

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
