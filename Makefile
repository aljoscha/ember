# Build ember (Rust) and, on macOS, ember-vz (Swift).
# Places both binaries side-by-side in target/{debug,release}/ so ember
# can find ember-vz at runtime.

UNAME := $(shell uname -s)

.PHONY: build release clean fmt check clippy test

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
	cargo check

clippy:
	cargo clippy -- -D warnings

test:
	cargo test
