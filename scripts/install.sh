#!/usr/bin/env bash
# Build TupleBase in release mode and install it for the current user (no sudo).
#   macOS: ~/Applications/TupleBase.app  (+ ~/.local/bin/tuplebase-mcp symlink)
#   Linux: ~/.local/bin, desktop entry + icon under ~/.local/share
# TB_BINS overrides which binaries are built (default: "tuplebase-app tuplebase-mcp").
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BINS="${TB_BINS:-tuplebase-app tuplebase-mcp}"
RELEASE="$ROOT/target/release"
BIN_DIR="$HOME/.local/bin"

pkg_args=()
for bin in $BINS; do pkg_args+=(-p "$bin"); done
(cd "$ROOT" && cargo build --release "${pkg_args[@]}")

has_mcp=false
[[ " $BINS " == *" tuplebase-mcp "* ]] && has_mcp=true

case "$(uname -s)" in
  Darwin)
    APP="$HOME/Applications/TupleBase.app"
    mkdir -p "$HOME/Applications"
    TB_BINS="$BINS" "$ROOT/scripts/bundle-macos.sh" "$APP"
    echo "Installed app: $APP"
    if $has_mcp; then
      mkdir -p "$BIN_DIR"
      ln -sf "$APP/Contents/MacOS/tuplebase-mcp" "$BIN_DIR/tuplebase-mcp"
      echo "Installed MCP: $BIN_DIR/tuplebase-mcp -> $APP/Contents/MacOS/tuplebase-mcp"
    fi
    ;;
  Linux)
    mkdir -p "$BIN_DIR" "$HOME/.local/share/icons/hicolor/256x256/apps" "$HOME/.local/share/applications"
    for bin in $BINS; do install -m 755 "$RELEASE/$bin" "$BIN_DIR/$bin"; done
    install -m 644 "$ROOT/assets/icon/icon-256.png" "$HOME/.local/share/icons/hicolor/256x256/apps/tuplebase.png"
    cat > "$HOME/.local/share/applications/tuplebase.desktop" <<DESKTOP
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
    $has_mcp && echo "Installed MCP: $BIN_DIR/tuplebase-mcp"
    ;;
  *)
    echo "Unsupported OS for this script. On Windows run: cargo build --release" >&2
    echo "Binaries end up in target\\release\\" >&2
    exit 1
    ;;
esac
