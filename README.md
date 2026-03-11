# Ember

Lightweight CLI for managing [Firecracker](https://firecracker-microvm.github.io/) microVMs with ZFS-backed storage. No daemon, no REST API — just a single binary.

## Requirements

| Dependency | Purpose |
|---|---|
| [Rust](https://rustup.rs/) | Build from source |
| [ZFS](https://openzfs.org/) | Storage (`zfs`/`zpool` CLI tools) |
| [Firecracker](https://github.com/firecracker-microvm/firecracker) | VM hypervisor |
| curl | Kernel download |
| iptables, iproute2, sysctl | Networking (TAP devices, NAT) |
| [skopeo](https://github.com/containers/skopeo) | OCI image pull |
| Docker or Podman | Image build (optional) |

Ember requires **root privileges** — ZFS, TAP devices, iptables, and Firecracker all need root. Like Docker, run with `sudo`.

## Building

```bash
cargo build --release
```

The binary is at `./target/release/ember`.

> [!NOTE]
> If you need to run Docker inside a VM, you'll need a custom kernel with additional networking modules. See [Building a custom kernel](#building-a-custom-kernel) below.

## Quick start

Initialize ember with a ZFS pool:

```bash
sudo ember init --pool mypool --device /dev/sdb
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

Build an image:

```bash
sudo ember image build ubuntu-dev
```

Or pull from an OCI registry:

```bash
sudo ember image pull docker.io/library/alpine:latest
```

Create and boot a VM:

```bash
sudo ember vm create myvm --image ubuntu-dev
```

SSH in:

```bash
ember ssh myvm
```

Run a command:

```bash
ember exec myvm -- uname -a
```

Stop the VM:

```bash
sudo ember vm stop myvm
```

## VM lifecycle

```bash
# Create (starts by default, use --no-start to skip)
sudo ember vm create myvm --image ubuntu-dev --cpus 2 --memory 4G --disk-size 16G

# Start / stop
sudo ember vm start myvm
sudo ember vm stop myvm
sudo ember vm stop myvm --force   # SIGKILL

# Pause / resume (Firecracker snapshot-based)
sudo ember vm pause myvm
sudo ember vm resume myvm

# Resize disk (grow only, VM must be stopped)
sudo ember vm resize myvm --disk-size 32G

# Delete
sudo ember vm delete myvm
sudo ember vm delete myvm --force   # force-kill if running

# List and inspect
ember vm list
ember vm list --format json
ember vm inspect myvm
```

Sizes use mandatory unit suffixes: `512M`, `4G`, `16G`, `2T` (binary, powers of 1024).

You can also pass a YAML config file instead of CLI flags:

```bash
sudo ember vm create myvm --vm-config vm.yaml
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

Fork is the primary way to duplicate VMs. It creates an instant copy-on-write clone via ZFS — no matter how large the disk, forking takes milliseconds. The forked VM is fully independent: you can modify, delete, or resize it without affecting the source.

Set up a base VM, then fork as many copies as you need:

```bash
# Build your golden image
sudo ember vm create base --image ubuntu-dev
ember ssh base
# ... install your apps, configure everything ...
sudo ember vm stop base

# Fork independent copies
sudo ember vm fork base worker-1
sudo ember vm fork base worker-2
sudo ember vm fork base worker-3
```

Each fork starts automatically and gets its own network identity. You can override resource allocation per fork:

```bash
sudo ember vm fork base beefy --cpus 4 --memory 32G --disk-size 64G
```

Forks can grow the disk but not shrink it below the source size. Use `--no-start` to fork without booting:

```bash
sudo ember vm fork base template --no-start
```

## Snapshots

Snapshots capture point-in-time state of a VM's disk. Useful for checkpointing before risky changes.

```bash
sudo ember snapshot create myvm before-upgrade
sudo ember snapshot list myvm

# Something went wrong? Roll back (VM must be stopped):
sudo ember vm stop myvm
sudo ember snapshot restore myvm before-upgrade
sudo ember vm start myvm

# Clean up:
sudo ember snapshot delete myvm before-upgrade
```

## Images

The default Dockerfile builds an Ubuntu 26.04 image with systemd, sshd, and a developer toolchain (Rust, Go, Claude Code, gh, jj, etc.). Use `-f` to build from a custom Dockerfile instead.

```bash
# Build from the default Dockerfile (Ubuntu 26.04 + systemd + sshd + dev toolchain)
sudo ember image build ubuntu-dev

# Build from a custom Dockerfile
sudo ember image build myimage -f ./Dockerfile

# Pull from an OCI registry
sudo ember image pull docker.io/library/alpine:latest

# List / inspect / delete
ember image list
ember image inspect ubuntu-dev
sudo ember image delete ubuntu-dev
sudo ember image delete ubuntu-dev --force   # cascade-deletes dependent VMs
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

## Building a custom kernel

The stock kernel (`vmlinux-6.1.102`, auto-downloaded on first use) works for most use cases. However, it **lacks full Docker networking support** — the iptables `raw` table and nftables modules are missing, so Docker bridge networking doesn't work inside guest VMs.

If you need Docker with bridge networking inside your VMs, build a custom kernel from the `kernel/` directory. It takes the Firecracker CI kernel config and merges a `docker.fragment` that adds the missing networking options (`CONFIG_IP_NF_RAW`, `CONFIG_NF_TABLES`, etc.).

### Native build

Requires: gcc, make, flex, bison, libelf-dev, libssl-dev, bc, git, curl, python3.

```bash
cd kernel
make
```

This will:
1. Download the Firecracker CI base config
2. Shallow-clone the Amazon Linux kernel source (6.1.163)
3. Merge the base config with `docker.fragment`
4. Compile `vmlinux`

### Docker build (reproducible)

No host dependencies needed beyond Docker:

```bash
cd kernel
make docker-build
```

Both methods produce `kernel/vmlinux`.

### Other make targets

```bash
make config       # merge configs without compiling
make clean        # remove build artifacts (keeps source)
make distclean    # remove everything including source
```

## Using a custom kernel

Pass the kernel path when creating a VM:

```bash
sudo ember vm create myvm --image ubuntu-dev --kernel ./kernel/vmlinux
```

In a YAML config:

```yaml
kernel: /path/to/kernel/vmlinux
```

Or set it as the default for all new VMs at init time:

```bash
sudo ember init --kernel /path/to/kernel/vmlinux
```
