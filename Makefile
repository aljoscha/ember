# Build ember (Rust) and, on macOS, ember-vz (Swift).
# Places both binaries side-by-side in target/{debug,release}/ so ember
# can find ember-vz at runtime.

UNAME := $(shell uname -s)

.PHONY: build release clean fmt check clippy test udeps emberd

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

# Build emberd (in-VM daemon). Runs inside Linux VMs so the vsock listener
# only compiles on Linux, but UDS-only mode works on macOS for testing.
emberd:
	cargo build -p emberd

emberd-release:
	cargo build -p emberd --release

clean:
	cargo clean
ifeq ($(UNAME),Darwin)
	cd ember-vz && swift package clean
endif

fmt:
	cargo fmt --all

check:
	cargo check --workspace

clippy:
	cargo clippy --workspace -- -D warnings

test:
	cargo test --workspace

udeps:
	cargo machete
