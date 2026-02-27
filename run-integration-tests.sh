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
#   ./run-integration-tests.sh image init   # run specific test files
#   ./run-integration-tests.sh vm::networking_ssh_and_internet  # run one test

set -euo pipefail

# Parse arguments. "vm::foo" means run only test "foo" from tests/vm.rs.
# Plain "vm" runs all tests in tests/vm.rs.
declare -A test_filters  # map: test_file -> filter (empty = all)

if [[ $# -gt 0 ]]; then
    tests=()
    for arg in "$@"; do
        if [[ "$arg" == *::* ]]; then
            file="${arg%%::*}"
            filter="${arg#*::}"
            tests+=("$file")
            test_filters["$file"]="$filter"
        else
            tests+=("$arg")
        fi
    done
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
# Tests run with --test-threads=1 because they have global side effects
# (TAP devices, iptables rules, ZFS pools). The crash-recovery reconciliation
# code scans all em-* TAP devices system-wide, so parallel tests with separate
# state directories would delete each other's TAP devices as "orphaned".
echo ""
echo "Running ${#binaries[@]} integration test(s) as root..."
echo ""

failed=0
for i in "${!tests[@]}"; do
    name="${tests[$i]}"
    bin="${binaries[$i]}"
    filter="${test_filters[$name]:-}"

    if [[ -n "$filter" ]]; then
        echo "=== $name::$filter ==="
    else
        echo "=== $name ==="
    fi

    if sudo "$bin" --ignored --test-threads=1 --nocapture "$filter"; then
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
