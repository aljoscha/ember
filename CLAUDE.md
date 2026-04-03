# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is ember?

A lightweight CLI for managing microVMs with copy-on-write storage. CLI-only — no daemon, no REST API.

- **Linux**: Firecracker (KVM) + ZFS zvols. See SPEC.md for the full design, TODO.md for the task list.
- **macOS**: Apple Virtualization Framework + APFS clones. See MACOS-SPEC.md for the design, MACOS-TODO.md for the task list.

## Current Focus

macOS support. Work through MACOS-TODO.md one task at a time. Implement, verify, check off the task, commit, stop.

## Approach

- Follow the design in MACOS-SPEC.md closely for macOS work, SPEC.md for Linux work.
- Platform-specific code lives behind backend traits (`VmBackend`, `StorageBackend`, `NetworkBackend`) with `#[cfg(target_os)]` compile-time selection.
- Shell out to platform tools (same philosophy as Linux): `ember-vz` (Swift helper for AVF), `hdiutil`, `diskutil`, `cp -c`, Homebrew `e2fsprogs`.
- When exploring for design or debugging, start producing actionable output (plans, hypotheses, code) early. Don't spend the whole session just reading code.
- Deliver complete implementations — do not silently cut scope, leave mocks in place of real logic, or substitute hardcoded data where dynamic generation or fetching is feasible. If a reference implementation solves a problem properly (e.g. generating data from a live source), the spec and implementation should do the same rather than settling for a static shortcut. When a full solution is impractical, say so explicitly and get agreement before reducing scope.
- Work through the TODO one task at a time. Implement, verify, check off the task, commit, stop.

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

## Rust Compilation

- Always run `cargo fmt` and `cargo build` after making edits before reporting success.
- When refactoring function signatures or types, grep for all call sites and update them in the same pass.
- Check visibility (`pub`) before accessing fields/methods from other modules.

## Coding Style & Conventions

- Idiomatic Rust: Result/Option, pattern matching, iterators, traits.
- `snake_case` for functions/variables, `PascalCase` for types, `SCREAMING_SNAKE_CASE` for constants.
- Use `thiserror` for library-style errors, `anyhow` for application-level error propagation.
- Prefer explicit error handling. Use `?` for propagation, not `.unwrap()`.
- Shell out to platform CLI tools — no fragile C library bindings. Linux: `zfs`/`zpool`/`iptables`. macOS: `hdiutil`/`diskutil`/`cp -c`/`ember-vz`.

## Debugging & Refactoring Approach

- When debugging build or test failures, start by reproducing the exact failing command locally and reading its output. Do not run generic checks in a shotgun approach.
- Run `cargo build` after each logical unit of change. Fix all compilation errors before editing the next file.
- If stuck after 3-4 investigation steps without progress, stop and summarize what you've tried and found, then ask for direction.

## Architecture

See SPEC.md (Linux) and MACOS-SPEC.md (macOS) for full architecture. Key modules:

```
src/
├── main.rs              # Entry point, CLI dispatch
├── cli/                 # Command implementations (shared across platforms)
├── backend/
│   ├── mod.rs           # Backend trait definitions + #[cfg] re-exports
│   ├── linux/           # Firecracker + ZFS + TAP + iptables
│   └── macos/           # AVF (ember-vz) + APFS clones + vmnet
├── zfs/                 # ZFS pool, dataset, volume, snapshot operations (Linux)
├── firecracker/         # API client, process management, config builder (Linux)
├── network/             # TAP devices, IP allocation, NAT rules (Linux)
├── image/               # OCI pull, ext4 creation, image registry (shared)
├── ssh/                 # SSH client, exec, file copy (shared, cross-platform)
├── state/               # JSON state store with file locking (shared)
└── config/              # YAML config parsing and merge (shared)
```

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
