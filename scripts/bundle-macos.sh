#!/usr/bin/env bash
# Assemble TupleBase.app from the binaries in target/release.
# Usage: scripts/bundle-macos.sh <path/to/TupleBase.app>
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$1"
BINS="${TB_BINS:-tuplebase-app tuplebase-mcp}"
VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml")"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
for bin in $BINS; do cp "$ROOT/target/release/$bin" "$APP/Contents/MacOS/"; done
cp "$ROOT/assets/icon/TupleBase.icns" "$ROOT/LICENSE" "$ROOT/THIRD-PARTY-NOTICES.txt" "$APP/Contents/Resources/"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>TupleBase</string>
  <key>CFBundleDisplayName</key><string>TupleBase</string>
  <key>CFBundleIdentifier</key><string>app.tuplebase.desktop</string>
  <key>CFBundleExecutable</key><string>tuplebase-app</string>
  <key>CFBundleIconFile</key><string>TupleBase</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
# Ad-hoc signature: gives the bundle a stable identity for Gatekeeper and the keychain.
codesign --force --deep -s - "$APP"
