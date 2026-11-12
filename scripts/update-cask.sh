#!/usr/bin/env bash
# Point the Homebrew cask at a release. Run by the Release workflow.
# Usage: scripts/update-cask.sh <version> <sha256 of the macOS DMG>
set -euo pipefail

VERSION="$1"
SHA="$2"
CASK="$(cd "$(dirname "$0")/.." && pwd)/Casks/tuplebase.rb"

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "version must look like 1.2.3, got: $VERSION" >&2
  exit 1
fi
if [[ ! "$SHA" =~ ^[0-9a-f]{64}$ ]]; then
  echo "not a sha256: $SHA" >&2
  exit 1
fi

sed -i.bak \
  -e "s/^  version \".*\"/  version \"$VERSION\"/" \
  -e "s/^  sha256 \".*\"/  sha256 \"$SHA\"/" "$CASK"
rm "$CASK.bak"

# A cask that no longer has these lines would otherwise keep pointing at an old release.
grep -q "^  version \"$VERSION\"$" "$CASK" && grep -q "^  sha256 \"$SHA\"$" "$CASK" || {
  echo "could not update $CASK" >&2
  exit 1
}
