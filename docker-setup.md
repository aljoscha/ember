# Docker Setup for Base Image

Steps required to get Docker working on this environment (Ubuntu 24.04, kernel 6.1.102).

## Install Script

All commands run as root (or with `sudo`):

```bash
# 1. Install Docker prerequisites and add Docker's official repo
apt-get update -qq
apt-get install -y -qq ca-certificates curl gnupg
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
  > /etc/apt/sources.list.d/docker.list
apt-get update -qq

# 2. Install Docker Engine + plugins
apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# 3. Switch iptables to legacy mode
#    The kernel (6.1.102) in this environment doesn't support nftables properly.
#    Without this, dockerd fails to create NAT chains on startup.
update-alternatives --set iptables /usr/sbin/iptables-legacy
update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy

# 4. Add default user to docker group (adjust "ubuntu" if your user differs)
usermod -aG docker ubuntu

# 5. Install a wrapper that defaults "docker run" to --network=host
#    The kernel lacks the iptables "raw" table, so bridge networking fails.
#    This wrapper auto-injects --network=host unless the user explicitly passes --network.
cat > /usr/local/bin/docker << 'WRAPPER'
#!/bin/bash
if [ "$1" = "run" ]; then
    has_network=false
    for arg in "$@"; do
        case "$arg" in
            --network|--network=*|--net|--net=*) has_network=true ;;
        esac
    done
    if [ "$has_network" = false ]; then
        set -- "${@:1:1}" --network=host "${@:2}"
    fi
fi
exec /usr/bin/docker "$@"
WRAPPER
chmod +x /usr/local/bin/docker
```

## Why Each Step Is Needed

| Step | Problem | Fix |
|---|---|---|
| iptables-legacy (step 3) | Kernel doesn't support nftables protocol; dockerd crashes on startup trying to create NAT chains | Switch to legacy iptables backend |
| `--network=host` wrapper (step 5) | Kernel lacks the iptables `raw` table; bridge networking fails at container start with "Table does not exist" | Default to host networking, which skips iptables raw rules entirely |

## Not Needed in the Base Image

- `systemctl start docker` — Docker will start on boot automatically once installed via the package's systemd unit.

## Limitations

- **No bridge networking**: The default Docker bridge network does not work due to the missing iptables `raw` table. Containers must use `--network=host` or `--network=none`. The wrapper handles this automatically for `docker run`.
- **No kernel module loading**: `modprobe` is unavailable and `/lib/modules/` is empty, so the missing kernel features (conntrack, nf_tables raw table) cannot be added at runtime.
- **Host networking implications**: Containers share the host's network namespace. There is no port isolation between containers, and container ports bind directly on the host. `-p` port mappings are ignored (all ports are already exposed).
