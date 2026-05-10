#!/usr/bin/env bash
# Start the Valhalla HTTP service on port 8002.
# Requires tiles to have been built first (nix run .#setup-valhalla).
set -euo pipefail

_CONFORM_DIR="${CONFORMANCE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
CONFORMANCE_WORK="${CONFORMANCE_WORK:-$_CONFORM_DIR/.work}"
CFG="$CONFORMANCE_WORK/valhalla/config.json"

if [ ! -f "$CFG" ]; then
  echo "Error: Valhalla config not found at $CFG"
  echo "Run 'nix run .#setup-valhalla' first."
  exit 1
fi

echo "[valhalla] Starting HTTP service on port 8002…"
exec valhalla_service "$CFG" 2
