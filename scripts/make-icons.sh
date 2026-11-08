#!/usr/bin/env bash
# Regenerate app icon rasters from assets/icon/icon.svg (macOS only: uses swift, sips, iconutil).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIR="$ROOT/assets/icon"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

swift "$ROOT/scripts/render-svg.swift" "$DIR/icon.svg" "$DIR/icon-1024.png" 1024
sips -z 256 256 "$DIR/icon-1024.png" --out "$DIR/icon-256.png" >/dev/null

ICONSET="$TMP/TupleBase.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$DIR/icon-1024.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$DIR/icon-1024.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$DIR/TupleBase.icns"

echo "Wrote $DIR/{icon-1024.png,icon-256.png,TupleBase.icns}"
