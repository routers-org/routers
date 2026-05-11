#!/usr/bin/env bash
# Start the FMM HTTP server on port 9090.
# Uses the STMATCH algorithm — only the road network shapefile is required
# (no UBODT pre-computation).  Run 'nix run .#setup-fmm' first.
set -euo pipefail

_CONFORM_DIR="${CONFORMANCE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
CONFORMANCE_WORK="${CONFORMANCE_WORK:-$_CONFORM_DIR/.work}"

FMM_SERVER="${FMM_SERVER:-$_CONFORM_DIR/external/fmm_server/build/fmm_server}"
FMM_NETWORK="$CONFORMANCE_WORK/network/roads.shp"

for f in "$FMM_SERVER" "$FMM_NETWORK"; do
  if [ ! -f "$f" ]; then
    echo "Error: required file not found: $f"
    echo "Run 'nix run .#setup-fmm' first."
    exit 1
  fi
done

echo "[fmm] Starting HTTP server on port ${FMM_PORT:-9090}..."
exec env \
  FMM_NETWORK="$FMM_NETWORK" \
  FMM_PORT="${FMM_PORT:-9090}" \
  "$FMM_SERVER"
