#!/usr/bin/env bash
#
# Build and run integration tests.
#
# Integration tests (tests/*.rs) are marked #[ignore] because they need
# platform-specific prerequisites (root + ZFS on Linux, APFS on macOS).
# Test files use #![cfg(target_os)] to compile only on their platform.
#
# On Linux, tests run under sudo (root required for ZFS + TAP + iptables).
# On macOS, tests run as the current user (no root needed for APFS clones).
#
# Usage:
#   ./run-integration-tests.sh                        # run all platform tests
#   ./run-integration-tests.sh init                   # run only tests/init.rs
#   ./run-integration-tests.sh macos_storage          # run only tests/macos_storage.rs
#   ./run-integration-tests.sh vm::networking_ssh      # run one test

set -euo pipefail

platform="$(uname -s)"

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
# Tests that are cfg'd out for this platform will compile as empty binaries.
binaries=()
test_names=()
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
    test_names+=("$name")
done

echo ""
echo "Running ${#binaries[@]} integration test(s) on ${platform}..."
echo ""

failed=0
skipped=0
for i in "${!test_names[@]}"; do
    name="${test_names[$i]}"
    bin="${binaries[$i]}"
    filter="${test_filters[$name]:-}"

    if [[ -n "$filter" ]]; then
        echo "=== $name::$filter ==="
    else
        echo "=== $name ==="
    fi

    # On Linux, tests require root (ZFS, TAP, iptables).
    # On macOS, tests run without root (APFS, vmnet shared mode).
    if [[ "$platform" == "Linux" ]]; then
        runner=(sudo "$bin" --ignored --test-threads=1 --nocapture)
    else
        runner=("$bin" --ignored --test-threads=1 --nocapture)
    fi

    if "${runner[@]}" "$filter"; then
        echo "--- $name: PASSED ---"
    else
        # Exit code from test binary: if 0 tests ran (all cfg'd out), it's
        # still a success. Check by listing tests first.
        count=$("$bin" --ignored --list 2>/dev/null | grep -c "^.*: test$" || true)
        if [[ "$count" -eq 0 ]]; then
            echo "--- $name: SKIPPED (no tests for this platform) ---"
            skipped=$((skipped + 1))
        else
            echo "--- $name: FAILED ---"
            failed=$((failed + 1))
        fi
    fi
    echo ""
done

echo "Results: $((${#binaries[@]} - failed - skipped)) passed, $failed failed, $skipped skipped."

if [[ $failed -gt 0 ]]; then
    exit 1
fi
