#!/usr/bin/env bash
# System libraries GPUI needs to compile on Ubuntu.
set -euo pipefail
sudo apt-get update -q
sudo apt-get install -yq \
  libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libfontconfig-dev libvulkan-dev
