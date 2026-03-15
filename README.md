# Ember

Lightweight CLI for managing microVMs with copy-on-write storage. No daemon, no REST API — just a single binary.

- **macOS**: [Apple Virtualization Framework](https://developer.apple.com/documentation/virtualization) + APFS clones. No root required.
- **Linux**: [Firecracker](https://firecracker-microvm.github.io/) (KVM) + ZFS zvols. Requires root.

## Installation

### macOS (Homebrew)

```bash
brew tap aljoscha/ember https://github.com/aljoscha/ember
brew install ember
```

This installs both `ember` and `ember-vz` (the Swift helper for Apple Virtualization Framework), plus runtime dependencies (`e2fsprogs`, `skopeo`). Requires macOS 13+ (Ventura) and Xcode Command Line Tools.

To install the latest development version instead of a tagged release:

```bash
brew install --HEAD aljoscha/ember/ember
```

### macOS (from source)

```bash
make release
```

Binaries are at `./target/release/ember` and `./target/release/ember-vz`.

### Linux (from source)

```bash
cargo build --release
```

The binary is at `./target/release/ember`.

#### Linux dependencies

| Dependency | Purpose |
|---|---|
| [Rust](https://rustup.rs/) | Build from source |
| [ZFS](https://openzfs.org/) | Storage (`zfs`/`zpool` CLI tools) |
| [Firecracker](https://github.com/firecracker-microvm/firecracker) | VM hypervisor |
| curl | Kernel download |
| iptables, iproute2, sysctl | Networking (TAP devices, NAT) |
| [skopeo](https://github.com/containers/skopeo) | OCI image pull |
| Docker or Podman | Image build + kernel build |

Linux requires **root privileges** — ZFS, TAP devices, iptables, and Firecracker all need root. Like Docker, run with `sudo`.

## Quick start

### macOS

```bash
ember init
ember kernel build -y
ember image build ubuntu-dev
ember vm create myvm --image ubuntu-dev
ember ssh myvm
```

No `sudo` needed. State is stored in `~/Library/Application Support/ember/`. Storage uses instant APFS copy-on-write clones — creating VMs and snapshots takes milliseconds regardless of disk size.

The kernel build requires Docker or Podman and enables Docker networking inside your VMs. It only needs to run once — the kernel is cached for all future VMs. If you don't need Docker inside VMs, skip the `kernel build` step and the stock kernel will be auto-downloaded on first use.

### Linux

```bash
sudo ember init --pool mypool --device /dev/sdb
sudo ember kernel build -y
sudo ember image build ubuntu-dev
sudo ember vm create myvm --image ubuntu-dev
ember ssh myvm
```

> [!TIP]
> **No spare disk?** You can back a zpool with regular files instead:
> ```bash
> truncate -s 100G /home/ember-data/zpool1.img /home/ember-data/zpool2.img
> sudo zpool create ember /home/ember-data/zpool1.img /home/ember-data/zpool2.img
> sudo ember init --pool ember
> ```
> File-backed pools aren't added to the ZFS cache by default, so after a reboot you need to re-import manually:
> ```bash
> sudo zpool import -d /home/ember-data ember
> ```

### Images

The default image (`ubuntu-dev`) is Ubuntu 26.04 with systemd, sshd, and a developer toolchain (Rust, Go, Claude Code, gh, jj, etc.). You can also build from a custom Dockerfile or pull from an OCI registry:

```bash
# Build from a custom Dockerfile
ember image build myimage -f ./Dockerfile

# Pull a minimal image from an OCI registry
ember image pull docker.io/library/alpine:latest
```

> [!NOTE]
> On Linux, prefix image and VM commands with `sudo`. On macOS, no `sudo` is needed.

## VM lifecycle

```bash
# Create (starts by default, use --no-start to skip)
ember vm create myvm --image ubuntu-dev --cpus 2 --memory 4G --disk-size 16G

# Start / stop
ember vm start myvm
ember vm stop myvm
ember vm stop myvm --force   # SIGKILL

# Pause / resume
ember vm pause myvm
ember vm resume myvm

# Resize disk (grow only, VM must be stopped)
ember vm resize myvm --disk-size 32G

# Update config (VM must be stopped)
ember vm update-config myvm --cpus 4 --memory 8G

# Delete
ember vm delete myvm
ember vm delete myvm --force   # force-kill if running

# List and inspect
ember vm list
ember vm list --format json
ember vm inspect myvm
```

Sizes use mandatory unit suffixes: `512M`, `4G`, `16G`, `2T` (binary, powers of 1024).

You can also pass a YAML config file instead of CLI flags:

```bash
ember vm create myvm --vm-config vm.yaml
```

```yaml
# vm.yaml
name: myvm
image: ubuntu-dev
cpus: 2
memory: 4G
disk_size: 16G
kernel: stock
network:
  subnet: 10.100.0.0/16
ssh:
  user: ubuntu
  key: ~/.ssh/id_ed25519
```

Merge order: defaults < global config < YAML < CLI flags.

## Forking VMs

Fork creates an instant copy-on-write clone — no matter how large the disk, forking takes milliseconds. The forked VM is fully independent.

```bash
# Build your golden image
ember vm create base --image ubuntu-dev
ember ssh base
# ... install your apps, configure everything ...
ember vm stop base

# Fork independent copies
ember vm fork base worker-1
ember vm fork base worker-2
ember vm fork base worker-3
```

Each fork starts automatically and gets its own network identity. Override resources per fork:

```bash
ember vm fork base beefy --cpus 4 --memory 32G --disk-size 64G
```

Forks can grow the disk but not shrink it below the source size. Use `--no-start` to fork without booting:

```bash
ember vm fork base template --no-start
```

## Snapshots

Snapshots capture point-in-time state of a VM's disk. Useful for checkpointing before risky changes.

```bash
ember snapshot create myvm before-upgrade
ember snapshot list myvm

# Something went wrong? Roll back (VM must be stopped):
ember vm stop myvm
ember snapshot restore myvm before-upgrade
ember vm start myvm

# Clean up:
ember snapshot delete myvm before-upgrade
```

## Guest access

SSH keys are auto-injected at image build and VM creation time. The SSH user is auto-detected (`ubuntu` if `/home/ubuntu` exists, otherwise `root`).

```bash
# Interactive shell
ember ssh myvm

# Run a command
ember exec myvm -- apt-get update
ember exec myvm --user root -- systemctl status docker

# Copy files
ember cp ./local-file.txt myvm:/tmp/
ember cp myvm:/var/log/syslog ./syslog.txt
```

## Storage efficiency

Both platforms use copy-on-write storage, so VMs and snapshots share disk blocks with their parent image. Check actual disk usage:

```bash
ember debug storage-efficiency
```

## Building a custom kernel

The stock kernel (auto-downloaded on first use) works for most use cases. However, it **lacks full Docker networking support** — the iptables `raw` table and nftables modules are missing, so Docker bridge networking doesn't work inside guest VMs.

If you need Docker with bridge networking inside your VMs, build a custom kernel:

### Using `ember kernel build` (recommended)

Requires Docker or Podman. The build runs inside a container — no host compiler toolchain needed.

```bash
ember kernel build
```

Use `-y` to skip the confirmation prompt:

```bash
ember kernel build -y
```

The built kernel becomes the default for new VMs. Fall back to the stock kernel with:

```bash
ember vm create myvm --image ubuntu-dev --kernel stock
```

List available kernels:

```bash
ember kernel list
```

### Manual build with Make

Alternatively, build directly from the `kernel/` directory:

**Native** (requires gcc, make, flex, bison, libelf-dev, libssl-dev, bc, git, curl, python3):

```bash
cd kernel
make
```

**Docker** (reproducible, no host deps beyond Docker):

```bash
cd kernel
make docker-build
```

Both produce `kernel/vmlinux`. Pass the path when creating a VM:

```bash
ember vm create myvm --image ubuntu-dev --kernel ./kernel/vmlinux
```

## Platform details

The CLI is identical on both platforms. Under the hood:

| | Linux | macOS |
|---|---|---|
| Hypervisor | Firecracker (KVM) | Apple Virtualization Framework |
| Storage | ZFS zvols + snapshots | APFS clones (`cp -c`) |
| Networking | TAP devices + iptables NAT | vmnet shared mode |
| Root required | Yes | No |
| State directory | `/var/lib/ember/` | `~/Library/Application Support/ember/` |

See [SPEC.md](SPEC.md) for the Linux architecture and [MACOS-SPEC.md](MACOS-SPEC.md) for the macOS architecture.

## Contributing

> [!WARNING]
> **Integration tests are not run in CI.** GitHub Actions runners lack the virtualization support ember needs: nested KVM is unreliable on Linux runners (Firecracker) and macOS runners don't support nested Apple Virtualization Framework. CI covers build, clippy, unit tests, and formatting only. Please run integration tests locally before submitting a PR:
> ```bash
> ./run-integration-tests.sh
> ```
> This requires root + ZFS + Firecracker on Linux, or ember-vz + an AVF kernel on macOS.
