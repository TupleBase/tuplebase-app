#!/usr/bin/env bash
# Set the workspace version, commit it to main and tag it. Run by the Release workflow.
# Usage: scripts/release-tag.sh 0.2.0
set -euo pipefail

VERSION="$1"
TAG="v$VERSION"

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "version must look like 1.2.3, got: $VERSION" >&2
  exit 1
fi
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "tag $TAG already exists" >&2
  exit 1
fi

sed -i.bak "/^\[workspace.package\]/,/^\[/s/^version *= *\".*\"/version = \"$VERSION\"/" Cargo.toml
rm Cargo.toml.bak
cargo update --workspace

git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git add Cargo.toml Cargo.lock
# Nothing to commit when releasing the version already in Cargo.toml.
git diff --cached --quiet || git commit -m "Release $TAG"
git tag "$TAG"
git push origin HEAD:main "$TAG"
