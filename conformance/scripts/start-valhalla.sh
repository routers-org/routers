#!/usr/bin/env bash
# Start the Valhalla HTTP service.
# Requires tiles to have been built first (nix run .#setup-valhalla).
# Valhalla loads all routing tiles into memory before serving, which can
# take 30-90s depending on the network size — the conform recipe allows 3 min.
set -euo pipefail

_CONFORM_DIR="${CONFORMANCE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
CONFORMANCE_WORK="${CONFORMANCE_WORK:-$_CONFORM_DIR/.work}"
VALHALLA_DIR="$CONFORMANCE_WORK/valhalla"
CFG="$VALHALLA_DIR/config.json"

if [ ! -f "$CFG" ]; then
  echo "Error: Valhalla config not found at $CFG"
  echo "Run 'nix run .#setup-valhalla' first."
  exit 1
fi

if [ ! -d "$VALHALLA_DIR/tiles" ]; then
  echo "Error: Valhalla tiles not found at $VALHALLA_DIR/tiles"
  echo "Run 'nix run .#setup-valhalla' first."
  exit 1
fi

echo "[valhalla] Starting HTTP service (loading tiles from $VALHALLA_DIR/tiles)..."
exec valhalla_service "$CFG" 2
