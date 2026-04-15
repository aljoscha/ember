# Migrate Image Builds from Docker to mkosi

This document specifies the changes needed to replace Docker-based image building (`ember image build`) with [mkosi](https://github.com/systemd/mkosi), a tool purpose-built for creating OS disk images.

## Motivation

ember currently uses Docker to build VM images: `docker build` produces a container image, `docker create` + `docker export` extracts the filesystem as a tarball, then ember packs it into ext4. This works but stretches Docker beyond its intended use:

- Docker's layer caching, union filesystems, and container isolation are unnecessary overhead for building a flat rootfs.
- The build → create → export → tar extract → ext4 pipeline has four intermediate artifacts that exist only to bridge the container/VM gap.
- File ownership requires `fakeroot` + `gtar` hacks on macOS because Docker export produces a tarball that non-root can't extract correctly.
- `rm -rf /var/lib/apt/lists/*` after every `apt-get install` is a Docker layer-size optimization that's meaningless for VM images — and the final Dockerfile layer has to `apt-get update` again to undo it.

mkosi wraps distribution package managers (apt, dnf, pacman, zypper) directly to produce raw disk images. It is designed for exactly this use case.

## Scope

Only `ember image build` changes. `ember image pull` (OCI image pull via skopeo) is unaffected — it serves a different purpose (pulling pre-built container images like Alpine for quick use).

## mkosi Overview

mkosi uses an INI-style configuration file (`mkosi.conf`) plus a directory tree:

```
images/ubuntu-dev/
├── mkosi.conf                 # Main config: distro, packages, output format
├── mkosi.extra/               # Files overlaid onto the image (≈ COPY in Dockerfile)
│   └── etc/
│       ├── sudoers.d/
│       │   └── ubuntu
│       ├── sysctl.d/
│       │   └── 50-ping.conf
│       ├── ssh/
│       │   └── sshd_config.d/
│       │       └── ember.conf
│       └── environment
├── mkosi.sandbox/             # Package manager config (repos, GPG keys) — not in final image
│   └── etc/
│       └── apt/
│           ├── sources.list.d/
│           │   ├── docker.sources
│           │   └── github-cli.sources
│           └── trusted.gpg.d/
│               ├── docker.gpg
│               └── github-cli.gpg
├── mkosi.postinst.chroot      # Post-install script (runs inside image as root)
└── mkosi.repart/              # Partition layout
    └── 00-root.conf
```

Key settings:

```ini
[Distribution]
Distribution=ubuntu
Release=resolute

[Output]
Format=disk
# Bootable=no means no kernel, no initrd, no bootloader, no ESP.
# The kernel is provided externally by Firecracker / AVF.
Bootable=no

[Content]
Packages=
    systemd
    systemd-sysv
    dbus
    libpam-systemd
    openssh-server
    iproute2
    ...
```

## What Changes

### 1. Image definition format: Dockerfile → mkosi config tree

Each image (ubuntu-dev, ubuntu-slim) becomes a directory under `images/` containing mkosi config files instead of a single Dockerfile.

**Package installation** moves from `RUN apt-get install` to declarative `Packages=` lists in `mkosi.conf`. No need for `rm -rf /var/lib/apt/lists/*` between installs — mkosi handles the package manager lifecycle.

**File injection** moves from inline `RUN echo ... > /etc/foo` and `RUN sed -i` to files in `mkosi.extra/` that mirror the filesystem hierarchy. For example, the sudoers config becomes a file at `mkosi.extra/etc/sudoers.d/ubuntu` containing `ubuntu ALL=(ALL) NOPASSWD:ALL`.

**Custom apt repositories** (Docker, GitHub CLI) move to `mkosi.sandbox/etc/apt/sources.list.d/` and `mkosi.sandbox/etc/apt/trusted.gpg.d/`. mkosi's package manager picks these up during the build but they don't end up in the final image by default — add them to `mkosi.extra/` as well if they should persist for the user.

**Per-user installs** (rustup, uv, Claude Code) stay as shell commands in `mkosi.postinst.chroot`:

```bash
#!/bin/bash
set -euo pipefail

# System-level setup
systemctl enable ssh.socket
systemctl enable serial-getty@ttyS0.service
systemctl enable serial-getty@hvc0.service
systemctl enable docker
systemctl disable systemd-resolved.service
systemctl disable systemd-networkd.service

# Create user
id -u ubuntu &>/dev/null || useradd -m -s /bin/bash ubuntu
usermod -aG sudo,docker ubuntu

echo 'root:ember' | chpasswd

# Per-user tools
su - ubuntu -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
su - ubuntu -c 'curl -LsSf https://astral.sh/uv/install.sh | sh'
su - ubuntu -c 'curl -fsSL https://claude.ai/install.sh | bash'

# Locale
sed -i 's/# en_US.UTF-8/en_US.UTF-8/' /etc/locale.gen && locale-gen

# Refresh apt index so users can apt install out of the box
apt-get update
```

This requires `WithNetwork=yes` in `mkosi.conf` so scripts can download installers.

**Partition layout** is defined in `mkosi.repart/00-root.conf`:

```ini
[Partition]
Type=root
Format=ext4
CopyFiles=/
Minimize=guess
```

`Minimize=guess` tells systemd-repart to size the partition to fit the contents (like the current `estimate_size_mib` logic). The exact sizing behavior may need tuning — mkosi/systemd-repart may add padding that differs from the current estimate.

### 2. Build backend: Docker → mkosi

`src/image/build.rs` changes from shelling out to `docker build` / `docker create` / `docker export` / `tar xf` to shelling out to `mkosi`. The pipeline simplifies:

**Before (Docker):**
```
docker build → docker create → docker export → tar xf → inject SSH keys → mkfs.ext4 → dd to storage
```

**After (mkosi):**
```
mkosi build → inject SSH keys → copy to storage
```

mkosi produces a raw disk image directly. No intermediate container, no tarball, no ext4 creation step. The `fakeroot`/`gtar` workaround on macOS also goes away (though mkosi itself requires Linux — see "macOS builds" below).

The new `build()` function:

1. Detect `mkosi` binary (error if missing).
2. Resolve image config directory (user-provided or built-in default under `images/`).
3. Run `mkosi --directory <config-dir> build --output-dir <work_dir>`.
4. The output is a raw disk image file — pass it directly to `storage.create_image_volume()`.

The `detect_container_tool()`, `check_fakeroot_tools()`, and `export_and_extract()` functions are removed. `sanitize_name()` stays.

### 3. SSH key + resolv.conf injection

Currently `inject_image_config()` runs after rootfs extraction. Two options:

**Option A: Move injection into mkosi.** Place SSH key injection in the postinst script or use `mkosi.extra/`. The resolv.conf can be placed in `mkosi.extra/etc/resolv.conf`. This is cleaner but makes the mkosi config depend on host state (the user's SSH public key).

**Option B: Keep post-build injection.** After mkosi produces the disk image, mount it (loop mount on Linux, `hdiutil attach` on macOS), inject files, unmount. This keeps the mkosi config static/reproducible and matches the current architecture where injection is a separate step.

Recommendation: **Option B** — keep injection separate. It's already implemented, it keeps mkosi configs reproducible, and it matches how `ember image pull` works (inject after download).

This means ember needs to mount the raw disk image after mkosi produces it. On Linux: `mount -o loop <image> <mountpoint>`. On macOS: `hdiutil attach -mountpoint <mountpoint> <image>`. Both are already patterns used elsewhere in ember.

### 4. CLI interface

The `ember image build` CLI stays the same but the `--file`/`-f` flag changes meaning:

**Before:** `-f` points to a Dockerfile.
**After:** `-f` points to a mkosi config directory (containing `mkosi.conf` etc.).

If `-f` is not provided, the built-in default image config is used. Instead of embedding a Dockerfile as a string constant (`DEFAULT_DOCKERFILE`), ember writes the default mkosi config tree to a temp directory.

Alternatively, ship the default mkosi configs as files in the `images/` directory (already the case for custom Dockerfiles) and point to them by default. This avoids embedding file trees as string constants.

```
ember image build ubuntu-dev                          # uses images/ubuntu-dev/
ember image build ubuntu-dev -f ./my-custom-image/    # uses custom mkosi config dir
```

### 5. Default image bundling

Currently, `Dockerfile.ubuntu-dev` is embedded in the binary via `include_str!`. A mkosi config is a directory tree, not a single file, so this approach doesn't directly work. Options:

**Option A: Embed files individually.** Use `include_str!` / `include_bytes!` for each file in the config tree and write them to a temp directory at build time. Workable but brittle as files are added/removed.

**Option B: Embed as a tar archive.** Bundle the config directory as a tarball at compile time (via `build.rs`) and extract at runtime. More robust.

**Option C: Don't embed.** Ship the `images/` directory alongside the binary and reference it at runtime. Simpler, but ember is currently a single static binary with no runtime data files.

Recommendation: **Option A or B** to preserve single-binary distribution. Option B is cleaner if the config tree grows.

### 6. Build caching

mkosi provides two caching mechanisms:

- **Package cache** (`PackageCacheDirectory=`): Caches downloaded .deb files. Set this to a persistent directory (e.g., `<state_dir>/mkosi-cache/`) so rebuilds don't re-download packages.
- **Incremental cache** (`Incremental=yes`): Snapshots the image after package installation. Subsequent builds restore from the snapshot, skipping apt entirely. Only postinst scripts re-run.

Docker's per-RUN layer cache is more granular, but mkosi's incremental mode is fast enough in practice since package download (cached) and installation are the slow parts.

### 7. Multi-architecture builds

mkosi supports cross-architecture builds via `Architecture=` (e.g., `arm64`, `x86-64`) using QEMU user-mode emulation + binfmt_misc, similar to Docker's `--platform` with buildx. The current `TARGETARCH` build arg in the Dockerfiles translates to mkosi's `Architecture=` setting.

## macOS Builds

**mkosi is Linux-only.** It requires `apt`/`dpkg` (or equivalent) and Linux-specific filesystem tools. It cannot run natively on macOS.

Options for macOS:

1. **Build inside a Linux VM.** ember already manages Linux VMs — a bootstrap VM could run mkosi. This adds a chicken-and-egg problem for the first image build (need a VM to build the image that VMs use).

2. **Build on Linux, distribute the image.** Pre-build images on Linux CI and distribute as artifacts. `ember image build` on macOS would download a pre-built image instead of building locally. This is the approach most cloud VM platforms take.

3. **Keep Docker as a fallback on macOS.** If mkosi isn't available (i.e., on macOS), fall back to the current Docker-based build pipeline. This is pragmatic but means maintaining two build paths.

4. **Run mkosi in a Docker container on macOS.** Use Docker (which runs a Linux VM on macOS) to run mkosi inside a container. Ironic but practical — Docker is only the execution environment, not the image builder. Requires a container image with mkosi + apt available.

Recommendation: **Option 3 or 4** for the initial migration. mkosi on Linux, Docker fallback on macOS. Over time, move to option 2 (pre-built images from CI) which eliminates local build dependencies entirely.

## Migration Plan

### Phase 1: mkosi configs alongside Dockerfiles

1. Create `images/ubuntu-dev/` and `images/ubuntu-slim/` mkosi config trees.
2. Verify they produce equivalent images to the current Dockerfiles by building both and comparing contents.
3. No code changes to ember yet.

### Phase 2: mkosi backend in ember

4. Add mkosi detection to `src/image/build.rs` alongside Docker detection.
5. Implement the mkosi build path: `mkosi build` → mount image → inject SSH keys → unmount → import to storage.
6. Add `--backend docker|mkosi` flag (or auto-detect: prefer mkosi if available, fall back to Docker).
7. Update tests.

### Phase 3: Docker removal

8. Once mkosi path is stable, remove Docker build code.
9. Move macOS to one of the macOS options above.
10. Remove `Dockerfile.ubuntu-dev` and `Dockerfile.ubuntu-slim`.

## Dependencies

- **mkosi v25+** (for `Bootable=no`, modern `systemd-repart` integration)
- **systemd-repart** (usually bundled with systemd, needed for disk image creation)
- **debootstrap** or **apt** (for Ubuntu image bootstrapping — mkosi calls these)
- On macOS: Docker (fallback) or a Linux VM with mkosi installed
