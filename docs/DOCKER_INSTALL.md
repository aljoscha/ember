# Docker Installation Instructions

> For Ubuntu 26.04 (Resolute Raccoon) on x86_64

## What Was Installed

| Component               | Version  |
|-------------------------|----------|
| Docker Engine (CE)      | 29.3.0   |
| containerd.io           | 2.2.1    |
| Docker CLI              | 29.3.0   |
| Docker Buildx Plugin    | 0.31.1   |
| Docker Compose Plugin   | 5.1.0    |
| Docker Rootless Extras  | 29.3.0   |

### Additional dependencies installed

`apparmor`, `iptables`, `nftables`, `pigz`, `slirp4netns`, `libslirp0`, `dbus-user-session`

## Installation Steps (for Dockerfile / provisioning script)

```bash
# 1. Install prerequisites
apt-get update && apt-get install -y ca-certificates curl gnupg

# 2. Add Docker's official GPG key
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc

# 3. Add Docker repository
echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
  > /etc/apt/sources.list.d/docker.list

# 4. Install Docker Engine and plugins
apt-get update && apt-get install -y \
  docker-ce \
  docker-ce-cli \
  containerd.io \
  docker-buildx-plugin \
  docker-compose-plugin

# 5. Enable and start Docker (skip in Dockerfile, needed for VM provisioning)
systemctl enable docker
systemctl start docker

# 6. Add your user to the docker group (for non-root access)
usermod -aG docker ubuntu
```

## Dockerfile RUN block equivalent

If adding to a Dockerfile that builds this machine image, combine the steps into a single `RUN`:

```dockerfile
RUN apt-get update \
  && apt-get install -y ca-certificates curl gnupg \
  && install -m 0755 -d /etc/apt/keyrings \
  && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc \
  && chmod a+r /etc/apt/keyrings/docker.asc \
  && echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
       > /etc/apt/sources.list.d/docker.list \
  && apt-get update \
  && apt-get install -y \
       docker-ce \
       docker-ce-cli \
       containerd.io \
       docker-buildx-plugin \
       docker-compose-plugin \
  && apt-get clean \
  && rm -rf /var/lib/apt/lists/*
```

> **Important:** If this is a Docker-in-Docker (DinD) scenario, you will also need
> `--privileged` or appropriate capabilities at runtime. See the
> [official DinD documentation](https://hub.docker.com/_/docker) for details.

## Implications and Limitations

### Kernel version (6.1.102)
The kernel is 6.1.x, which fully supports overlayfs, cgroups v2, and user namespaces.
No kernel-level limitations for Docker.

### Storage driver: overlayfs
Docker auto-selected `overlayfs`, which is the recommended and most performant driver.
No changes needed.

### Cgroup v2 with systemd driver
The system uses cgroups v2 with the systemd cgroup driver. This is the modern default
and fully supported by Docker and containerd.

### Docker-in-Docker (DinD) considerations
If this machine image is itself built from a Docker container, running Docker inside it
requires either:
- `--privileged` flag (not recommended for production), or
- Specific capabilities: `SYS_ADMIN`, `NET_ADMIN`, plus a writable `/var/lib/docker`

### Rootless mode available
`docker-ce-rootless-extras` and `slirp4netns` were installed, so rootless Docker is
available if needed. Run `dockerd-rootless-setuptool.sh install` as a non-root user
to configure it.

### Security
- Docker daemon runs as root by default. The `ubuntu` user was added to the `docker`
  group, which effectively grants root-equivalent access.
- AppArmor was installed and enabled as a dependency — Docker uses it for container
  security profiles.

### Network
- `iptables` and `nftables` were installed. Docker uses nftables (via iptables-nft
  backend) for container networking rules.
- The nftables error in the startup log (`No such file or directory` for `docker-bridges`
  table) is benign — it occurs on first boot when no prior rules exist.
