#!/usr/bin/env bash
# Remove everything scripts/install.sh created. Config and credentials are left in place.
set -euo pipefail

BIN_DIR="$HOME/.local/bin"

case "$(uname -s)" in
  Darwin)
    rm -rf "$HOME/Applications/TupleBase.app"
    # Only remove the symlink if it points into the bundle we installed.
    if [[ -L "$BIN_DIR/tuplebase-mcp" && "$(readlink "$BIN_DIR/tuplebase-mcp")" == *"/TupleBase.app/"* ]]; then
      rm -f "$BIN_DIR/tuplebase-mcp"
    fi
    ;;
  Linux)
    rm -f "$BIN_DIR/tuplebase-app" "$BIN_DIR/tuplebase-mcp" \
      "$HOME/.local/share/icons/hicolor/256x256/apps/tuplebase.png" \
      "$HOME/.local/share/applications/tuplebase.desktop"
    ;;
  *)
    echo "Unsupported OS for this script." >&2
    exit 1
    ;;
esac
echo "TupleBase uninstalled."
