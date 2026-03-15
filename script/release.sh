#!/usr/bin/env bash
# Create a versioned release and update the Homebrew formula.
#
# Usage:
#   script/release.sh 0.2.0
#
# What it does:
#   1. Updates Cargo.toml version
#   2. Updates Formula/ember.rb version + URL
#   3. Creates a git tag (v0.2.0)
#   4. Pushes the tag to origin
#   5. Creates a GitHub release via `gh`
#   6. Downloads the release tarball and computes sha256
#   7. Updates Formula/ember.rb with the real sha256
#   8. Commits the sha256 update
#
# Prerequisites: gh (GitHub CLI), jq, shasum

set -euo pipefail

VERSION="${1:?Usage: $0 <version>  (e.g. 0.2.0)}"
TAG="v${VERSION}"
REPO="aljoscha/ember"
FORMULA="Formula/ember.rb"
CARGO_TOML="Cargo.toml"

echo "==> Releasing ${TAG}"

# Sanity checks
command -v gh >/dev/null || { echo "error: gh (GitHub CLI) not found"; exit 1; }
command -v jq >/dev/null || { echo "error: jq not found"; exit 1; }
command -v shasum >/dev/null || { echo "error: shasum not found"; exit 1; }

# 1. Update Cargo.toml version
sed -i '' "s/^version = \".*\"/version = \"${VERSION}\"/" "${CARGO_TOML}"
echo "  Updated ${CARGO_TOML} to ${VERSION}"

# 2. Update formula URL and version (sha256 will be a placeholder for now)
sed -i '' \
  -e "s|url \"https://github.com/${REPO}/archive/refs/tags/.*\.tar\.gz\"|url \"https://github.com/${REPO}/archive/refs/tags/${TAG}.tar.gz\"|" \
  -e "s/sha256 \"[a-f0-9]*\"/sha256 \"0000000000000000000000000000000000000000000000000000000000000000\"/" \
  "${FORMULA}"
echo "  Updated ${FORMULA} URL to ${TAG}"

# 3. Commit, tag, push
git add "${CARGO_TOML}" "${FORMULA}"
git commit -m "release: ${TAG}"
git tag "${TAG}"
git push origin main "${TAG}"
echo "  Pushed tag ${TAG}"

# 4. Create GitHub release
gh release create "${TAG}" \
  --title "${TAG}" \
  --notes "See [README.md](https://github.com/${REPO}#readme) for installation instructions." \
  --generate-notes
echo "  Created GitHub release ${TAG}"

# 5. Download tarball and compute sha256
TARBALL_URL="https://github.com/${REPO}/archive/refs/tags/${TAG}.tar.gz"
TMPFILE="$(mktemp)"
curl -sL "${TARBALL_URL}" -o "${TMPFILE}"
SHA256="$(shasum -a 256 "${TMPFILE}" | awk '{print $1}')"
rm -f "${TMPFILE}"
echo "  SHA256: ${SHA256}"

# 6. Update formula with real sha256
sed -i '' "s/sha256 \"0000000000000000000000000000000000000000000000000000000000000000\"/sha256 \"${SHA256}\"/" "${FORMULA}"

# 7. Commit the sha256 update
git add "${FORMULA}"
git commit -m "brew: update formula sha256 for ${TAG}"
git push origin main
echo "  Updated formula sha256"

echo ""
echo "==> Done! Users can now install with:"
echo "    brew tap aljoscha/ember https://github.com/aljoscha/ember"
echo "    brew install ember"
