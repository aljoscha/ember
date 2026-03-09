# Kernel build for ember microVMs
#
# Produces a kernel binary based on the Firecracker CI kernel config with
# extra options merged from config fragments.
#
# On x86_64 the output is an ELF vmlinux. On arm64 the output is the raw
# arch/arm64/boot/Image (what VZLinuxBootLoader and Firecracker expect).
#
# Usage:
#   make                           # full build (x86_64, docker fragment)
#   make ARCH=arm64 FRAGMENTS="avf.fragment"  # aarch64 AVF kernel
#   make docker-build              # reproducible build inside Docker
#   make fetch-base                # download the Firecracker CI base config
#   make fetch-source              # shallow-clone the Amazon Linux kernel source
#   make config                    # merge base config + fragments
#   make vmlinux                   # compile the kernel
#   make clean                     # remove build artifacts (keeps source and base config)
#   make distclean                 # remove everything including source

KERNEL_TAG    := microvm-kernel-6.1.163-20.299.amzn2023
KERNEL_REPO   := https://github.com/amazonlinux/linux.git
KERNEL_DIR    := linux
TAG_STAMP     := $(KERNEL_DIR)/.tag-$(KERNEL_TAG)
BASE_CONFIG   := base.config
OUTPUT        := vmlinux

# Architecture: x86_64 (default) or arm64.
# Maps to the Firecracker CI config name and the kernel ARCH variable.
ARCH          ?= x86_64

ifeq ($(ARCH),arm64)
  KARCH       := arm64
  FC_ARCH     := aarch64
  CROSS       := ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu-
else ifeq ($(ARCH),aarch64)
  # Accept aarch64 as an alias
  KARCH       := arm64
  FC_ARCH     := aarch64
  CROSS       := ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu-
else
  KARCH       := x86
  FC_ARCH     := x86_64
  CROSS       :=
endif

# On arm64, VZLinuxBootLoader and Firecracker both expect the raw Image
# format, not an ELF vmlinux. The Firecracker CI "vmlinux" for aarch64
# is actually this Image format too.
ifeq ($(KARCH),arm64)
  KERNEL_TARGET := Image
  KERNEL_PATH   := $(KERNEL_DIR)/arch/arm64/boot/Image
else
  KERNEL_TARGET := vmlinux
  KERNEL_PATH   := $(KERNEL_DIR)/vmlinux
endif

# Config fragments to merge on top of the base config.
# Default: docker.fragment (Docker bridge networking support).
# For AVF: FRAGMENTS="avf.fragment" (or combine: "docker.fragment avf.fragment")
FRAGMENTS     ?= docker.fragment

# Firecracker CI kernel config from their GitHub repo
BASE_CONFIG_URL := https://raw.githubusercontent.com/firecracker-microvm/firecracker/main/resources/guest_configs/microvm-kernel-ci-$(FC_ARCH)-6.1.config

# Docker image for reproducible builds
BUILDER_IMAGE := ember-kernel-builder
DOCKER        := docker

# Number of parallel jobs (default: all cores)
NPROC         := $(shell nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)

.PHONY: all fetch-base fetch-source config vmlinux clean distclean docker-build

all: vmlinux

# --- Fetch the Firecracker CI base config ---

$(BASE_CONFIG):
	@echo "==> Downloading Firecracker CI kernel config ($(FC_ARCH))..."
	curl -fSL -o $@ $(BASE_CONFIG_URL)

fetch-base: $(BASE_CONFIG)

# --- Fetch the kernel source ---
# The stamp file encodes the tag name so that changing KERNEL_TAG
# triggers a fresh clone (the old stamp won't exist).

$(TAG_STAMP):
	@if [ -d $(KERNEL_DIR) ]; then \
		echo "==> KERNEL_TAG changed, removing stale source..."; \
		rm -rf $(KERNEL_DIR); \
	fi
	@echo "==> Cloning kernel source (shallow, tag $(KERNEL_TAG))..."
	git clone --depth 1 --branch $(KERNEL_TAG) $(KERNEL_REPO) $(KERNEL_DIR)
	@touch $@

fetch-source: $(TAG_STAMP)

# --- Merge configs ---

$(KERNEL_DIR)/.config: $(BASE_CONFIG) $(FRAGMENTS) $(TAG_STAMP)
	@echo "==> Merging base config + $(FRAGMENTS)..."
	cd $(KERNEL_DIR) && \
		KCONFIG_CONFIG=.config \
		scripts/kconfig/merge_config.sh -m $(addprefix ../,$(BASE_CONFIG) $(FRAGMENTS))
	@echo "==> Patching BUILD_SALT to match kernel source..."
	sed -i 's/^CONFIG_BUILD_SALT=.*/CONFIG_BUILD_SALT=""/' $(KERNEL_DIR)/.config
	cd $(KERNEL_DIR) && make $(CROSS) olddefconfig

config: $(KERNEL_DIR)/.config

# --- Build kernel (native or cross) ---

$(KERNEL_PATH): $(KERNEL_DIR)/.config
	@echo "==> Building $(KERNEL_TARGET) ($(FC_ARCH), $(NPROC) jobs)..."
	cd $(KERNEL_DIR) && make $(CROSS) -j$(NPROC) $(KERNEL_TARGET)

$(OUTPUT): $(KERNEL_PATH)
	cp $(KERNEL_PATH) $(OUTPUT)
	@echo "==> Kernel ready: $(OUTPUT)"

vmlinux: $(OUTPUT)

# --- Build inside Docker (reproducible) ---

docker-build:
	$(DOCKER) build --platform linux/$(FC_ARCH) -t $(BUILDER_IMAGE) .
	$(DOCKER) run --rm --platform linux/$(FC_ARCH) \
		--user $$(id -u):$$(id -g) \
		-v $(CURDIR):/build \
		-w /build \
		$(BUILDER_IMAGE) \
		make vmlinux ARCH=$(ARCH) FRAGMENTS="$(FRAGMENTS)" NPROC=$(NPROC)

# --- Clean ---

clean:
	rm -f $(OUTPUT)
	-[ -d $(KERNEL_DIR) ] && cd $(KERNEL_DIR) && make mrproper

distclean:
	rm -f $(OUTPUT)
	rm -rf $(KERNEL_DIR) $(BASE_CONFIG) 2>/dev/null; \
	if [ -d $(KERNEL_DIR) ]; then \
		echo "==> Root-owned files detected, cleaning via Docker..."; \
		$(DOCKER) run --rm -v $(CURDIR):/build -w /build alpine \
			rm -rf /build/$(KERNEL_DIR); \
	fi
