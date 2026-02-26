#!/usr/bin/env bash
#
# Build and run integration tests under sudo.
#
# Integration tests (tests/*.rs) are marked #[ignore] because they need
# root, ZFS, and (for image tests) network + skopeo.  This script builds
# them as the current user, extracts the exact binary path from cargo's
# JSON output, and runs each one under sudo.
#
# Usage:
#   ./run-integration-tests.sh              # run all integration tests
#   ./run-integration-tests.sh init         # run only tests/init.rs
#   ./run-integration-tests.sh image init   # run specific tests

set -euo pipefail

# Discover which test crates to run.
if [[ $# -gt 0 ]]; then
    tests=("$@")
else
    # All .rs files in tests/ (strip path and extension).
    tests=()
    for f in tests/*.rs; do
        [[ -f "$f" ]] || continue
        name="$(basename "$f" .rs)"
        tests+=("$name")
    done
fi

if [[ ${#tests[@]} -eq 0 ]]; then
    echo "No integration tests found in tests/" >&2
    exit 1
fi

# Build all requested test crates and collect their binary paths.
binaries=()
for name in "${tests[@]}"; do
    echo "Building test: $name"
    bin=$(cargo test --test "$name" --no-run --message-format=json 2>/dev/null \
        | jq -r 'select(.reason == "compiler-artifact" and .target.kind == ["test"]) | .executable')

    if [[ -z "$bin" || ! -x "$bin" ]]; then
        echo "  ERROR: failed to find executable for test '$name'" >&2
        exit 1
    fi
    echo "  Binary: $bin"
    binaries+=("$bin")
done

# Run each test binary under sudo.
echo ""
echo "Running ${#binaries[@]} integration test(s) as root..."
echo ""

failed=0
for i in "${!tests[@]}"; do
    name="${tests[$i]}"
    bin="${binaries[$i]}"
    echo "=== $name ==="
    if sudo "$bin" --ignored; then
        echo "--- $name: PASSED ---"
    else
        echo "--- $name: FAILED ---"
        failed=$((failed + 1))
    fi
    echo ""
done

if [[ $failed -gt 0 ]]; then
    echo "$failed test suite(s) failed."
    exit 1
else
    echo "All integration tests passed."
fi
