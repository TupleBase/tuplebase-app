#!/usr/bin/env bash
# Package the release binaries into dist/ for a GitHub Release.
# Usage: scripts/package.sh <platform-name>   e.g. macos-arm64, linux-x64, windows-x64
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PLATFORM="$1"
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml")"
NAME="TupleBase-$VERSION-$PLATFORM"
DIST="$ROOT/dist"
STAGE="$ROOT/target/stage/$NAME"

rm -rf "$STAGE"
mkdir -p "$STAGE" "$DIST"

case "$PLATFORM" in
  macos-*)
    "$ROOT/scripts/bundle-macos.sh" "$STAGE/TupleBase.app"
    ln -s /Applications "$STAGE/Applications"
    hdiutil create -volname TupleBase -srcfolder "$STAGE" -ov -format UDZO "$DIST/$NAME.dmg"
    ;;
  linux-*)
    cp "$ROOT"/target/release/tuplebase-{app,mcp} "$STAGE/"
    cp "$ROOT/assets/icon/icon-256.png" "$STAGE/tuplebase.png"
    tar -C "$(dirname "$STAGE")" -czf "$DIST/$NAME.tar.gz" "$NAME"
    ;;
  windows-*)
    cp "$ROOT"/target/release/tuplebase-{app,mcp}.exe "$STAGE/"
    (cd "$STAGE" && 7z a -tzip "$DIST/$NAME.zip" . >/dev/null)
    ;;
  *)
    echo "unknown platform: $PLATFORM" >&2
    exit 1
    ;;
esac
