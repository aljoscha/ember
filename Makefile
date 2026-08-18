# Build ember (Rust) and, on macOS, ember-vz (Swift).
# Places both binaries side-by-side in target/{debug,release}/ so ember
# can find ember-vz at runtime.

UNAME := $(shell uname -s)

# Which workspace members to check, lint, and test.
#
# `default-members = ["."]` in Cargo.toml means a bare `cargo test` only
# covers the root package, so the unit tests in ember-core and the
# platform crate never run. `--workspace` is not the fix: it selects the
# other platform's backend too, and that backend does not compile here
# (ember-macos needs `clonefile`, ember-linux's image tests need
# `mkfs.ext4`). Naming the packages is what actually works.
ifeq ($(UNAME),Darwin)
PACKAGES := -p ember -p ember-core -p ember-macos
else
PACKAGES := -p ember -p ember-core -p ember-linux
endif

.PHONY: build release clean fmt check clippy test udeps

build:
	cargo build
ifeq ($(UNAME),Darwin)
	cd ember-vz && swift build
	codesign --force --sign - --entitlements ember-vz/entitlements.plist ember-vz/.build/debug/ember-vz
	cp ember-vz/.build/debug/ember-vz target/debug/
endif

release:
	cargo build --release
ifeq ($(UNAME),Darwin)
	cd ember-vz && swift build -c release
	codesign --force --sign - --entitlements ember-vz/entitlements.plist ember-vz/.build/release/ember-vz
	cp ember-vz/.build/release/ember-vz target/release/
endif

clean:
	cargo clean
ifeq ($(UNAME),Darwin)
	cd ember-vz && swift package clean
endif

fmt:
	cargo fmt

check:
	cargo check --all-targets $(PACKAGES)

clippy:
	cargo clippy --all-targets $(PACKAGES) -- -D warnings

test:
	cargo test $(PACKAGES)

udeps:
	cargo machete
