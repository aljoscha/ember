# Kernel build for ember microVMs
#
# Produces a vmlinux binary based on the Firecracker CI kernel config with
# extra options for Docker bridge networking support.
#
# Usage:
#   make              # full build (fetch source, merge config, compile)
#   make fetch-base   # download the Firecracker CI base config
#   make fetch-source # shallow-clone the Amazon Linux kernel source
#   make config       # merge base config + docker.fragment
#   make vmlinux      # compile the kernel
#   make clean        # remove build artifacts (keeps source and base config)
#   make distclean    # remove everything including source

KERNEL_TAG    := microvm-kernel-6.1.102-1.182.amzn2023
KERNEL_REPO   := https://github.com/amazonlinux/linux.git
KERNEL_DIR    := linux
BASE_CONFIG   := base.config
FRAGMENT      := docker.fragment
OUTPUT        := vmlinux

# Firecracker CI kernel config from their GitHub repo (x86_64)
BASE_CONFIG_URL := https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-x86_64-6.1.config

# Docker image for reproducible builds
BUILDER_IMAGE := ember-kernel-builder
DOCKER        := docker

# Number of parallel jobs (default: all cores)
NPROC         := $(shell nproc 2>/dev/null || echo 4)

.PHONY: all fetch-base fetch-source config vmlinux clean distclean docker-build

all: vmlinux

# --- Fetch the Firecracker CI base config ---

$(BASE_CONFIG):
	@echo "==> Downloading Firecracker CI kernel config..."
	curl -fSL -o $@ $(BASE_CONFIG_URL)

fetch-base: $(BASE_CONFIG)

# --- Fetch the kernel source ---

$(KERNEL_DIR)/.git:
	@echo "==> Cloning kernel source (shallow, tag $(KERNEL_TAG))..."
	git clone --depth 1 --branch $(KERNEL_TAG) $(KERNEL_REPO) $(KERNEL_DIR)

fetch-source: $(KERNEL_DIR)/.git

# --- Merge configs ---

$(KERNEL_DIR)/.config: $(BASE_CONFIG) $(FRAGMENT) $(KERNEL_DIR)/.git
	@echo "==> Merging base config + docker.fragment..."
	cd $(KERNEL_DIR) && \
		KCONFIG_CONFIG=.config \
		scripts/kconfig/merge_config.sh -m ../$(BASE_CONFIG) ../$(FRAGMENT)
	cd $(KERNEL_DIR) && make olddefconfig

config: $(KERNEL_DIR)/.config

# --- Build vmlinux (native) ---

$(KERNEL_DIR)/vmlinux: $(KERNEL_DIR)/.config
	@echo "==> Building vmlinux ($(NPROC) jobs)..."
	cd $(KERNEL_DIR) && make -j$(NPROC) vmlinux

$(OUTPUT): $(KERNEL_DIR)/vmlinux
	cp $(KERNEL_DIR)/vmlinux $(OUTPUT)
	@echo "==> Kernel ready: $(OUTPUT)"

vmlinux: $(OUTPUT)

# --- Build inside Docker (reproducible) ---

docker-build:
	$(DOCKER) build -t $(BUILDER_IMAGE) .
	$(DOCKER) run --rm \
		-v $(CURDIR):/build \
		-w /build \
		$(BUILDER_IMAGE) \
		make vmlinux NPROC=$(NPROC)

# --- Clean ---

clean:
	rm -f $(OUTPUT)
	[ -d $(KERNEL_DIR) ] && cd $(KERNEL_DIR) && make mrproper || true

distclean: clean
	rm -rf $(KERNEL_DIR) $(BASE_CONFIG)
