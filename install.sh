#!/bin/sh
# Install the latest TupleBase release for the current user, without sudo.
#
#   curl -fsSL https://raw.githubusercontent.com/TupleBase/tuplebase-app/main/install.sh | sh
#
# Set TUPLEBASE_VERSION=x.y.z to install a specific version. The app also sets
# TUPLEBASE_APP_PATH or TUPLEBASE_BIN_DIR to update itself where it is installed, and
# TUPLEBASE_DOWNLOAD_DIR when it has already downloaded the release files.
set -eu

REPO_URL="https://github.com/TupleBase/tuplebase-app"
BIN_DIR="${TUPLEBASE_BIN_DIR:-$HOME/.local/bin}"

fail() {
  echo "error: $1" >&2
  exit 1
}

latest_version() {
  # GitHub redirects /releases/latest to /releases/tag/v<version>.
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$REPO_URL/releases/latest") ||
    fail "could not reach GitHub"
  echo "${url##*/v}"
}

verify() {
  if command -v sha256sum >/dev/null 2>&1; then
    check="sha256sum -c -"
  else
    check="shasum -a 256 -c -"
  fi
  grep " $1\$" SHA256SUMS | $check >/dev/null || fail "checksum mismatch for $1"
}

cleanup() {
  if [ -n "${mounted:-}" ]; then
    hdiutil detach -quiet "$mnt" </dev/null || true
  fi
  rm -rf "$tmp"
}

install_macos() {
  app="${TUPLEBASE_APP_PATH:-$HOME/Applications/TupleBase.app}"
  mnt="$tmp/mnt"
  mkdir -p "$mnt" "$(dirname "$app")" "$BIN_DIR"
  hdiutil attach -nobrowse -quiet -readonly -mountpoint "$mnt" "$1" </dev/null
  mounted=1
  # Copy first, so a failure can't leave you without an app.
  rm -rf "$app.new"
  ditto "$mnt/TupleBase.app" "$app.new"
  rm -rf "$app"
  mv "$app.new" "$app"
  ln -sf "$app/Contents/MacOS/tuplebase-mcp" "$BIN_DIR/tuplebase-mcp"
  echo "Installed app: $app"
}

install_linux() {
  share="${XDG_DATA_HOME:-$HOME/.local/share}"
  tar -xzf "$1"
  install -Dm755 "$2/tuplebase-app" "$BIN_DIR/tuplebase-app"
  install -Dm755 "$2/tuplebase-mcp" "$BIN_DIR/tuplebase-mcp"
  install -Dm644 "$2/tuplebase.png" "$share/icons/hicolor/256x256/apps/tuplebase.png"
  for doc in LICENSE THIRD-PARTY-NOTICES.txt; do
    if [ -f "$2/$doc" ]; then
      install -Dm644 "$2/$doc" "$share/doc/tuplebase/$doc"
    fi
  done
  mkdir -p "$share/applications"
  cat >"$share/applications/tuplebase.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=TupleBase
Comment=Database client
Exec=$BIN_DIR/tuplebase-app
Icon=tuplebase
Categories=Development;Database;
Terminal=false
DESKTOP
  echo "Installed app: $BIN_DIR/tuplebase-app (menu entry: TupleBase)"
}

# Everything runs from here, so a truncated download cannot run half a script.
main() {
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) platform=macos-arm64 ext=dmg ;;
    Linux-x86_64) platform=linux-x64 ext=tar.gz ;;
    *) fail "no build for $(uname -s) $(uname -m), see $REPO_URL/blob/main/docs/INSTALL.md" ;;
  esac

  version="${TUPLEBASE_VERSION:-$(latest_version)}"
  case "$version" in
    "" | *[!0-9.]*) fail "no release found" ;;
  esac

  name="TupleBase-$version-$platform"
  file="$name.$ext"
  base="$REPO_URL/releases/download/v$version"

  tmp=$(mktemp -d)
  trap cleanup EXIT
  trap 'exit 1' INT TERM
  cd "$tmp"

  if [ -n "${TUPLEBASE_DOWNLOAD_DIR:-}" ]; then
    cp "$TUPLEBASE_DOWNLOAD_DIR/$file" "$TUPLEBASE_DOWNLOAD_DIR/SHA256SUMS" . ||
      fail "could not find $file in $TUPLEBASE_DOWNLOAD_DIR"
  else
    echo "Downloading TupleBase $version..."
    curl -fsSLO "$base/$file" || fail "could not download $file"
    curl -fsSLO "$base/SHA256SUMS" || fail "could not download SHA256SUMS"
  fi
  verify "$file"

  case "$platform" in
    macos-*) install_macos "$file" ;;
    linux-*) install_linux "$file" "$name" ;;
  esac

  echo "Installed MCP: $BIN_DIR/tuplebase-mcp"
  case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "Note: $BIN_DIR is not on your PATH." ;;
  esac
}

main
