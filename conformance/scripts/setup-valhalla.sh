#!/usr/bin/env bash
# Build Valhalla routing tiles from the configured PBF.
# Run once; re-run only if the network data changes.
#
# Environment variables (all have defaults):
#   CONFORMANCE_PBF   path to the .osm.pbf file
#   CONFORMANCE_WORK  writable work directory (default: $PWD/.work)
set -euo pipefail

# CONFORMANCE_DIR is the conformance/ package root.  When invoked via
# `nix run`, $0 is in the nix store so we rely on the caller (the nix app
# wrapper) to export CONFORMANCE_DIR=$PWD.  Direct bash invocation falls
# back to computing it from the script's own location.
_CONFORM_DIR="${CONFORMANCE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
CONFORMANCE_PBF="${CONFORMANCE_PBF:-$_CONFORM_DIR/../libs/routers_fixtures/resources/los-angeles-minified.osm.pbf}"
CONFORMANCE_WORK="${CONFORMANCE_WORK:-$_CONFORM_DIR/.work}"
VALHALLA_DIR="$CONFORMANCE_WORK/valhalla"
CFG="$VALHALLA_DIR/config.json"

if [ ! -f "$CONFORMANCE_PBF" ]; then
  echo "Error: PBF not found at $CONFORMANCE_PBF"
  echo "Set CONFORMANCE_PBF to point at your .osm.pbf file."
  exit 1
fi

mkdir -p "$VALHALLA_DIR/tiles"

echo "[setup-valhalla] Generating config…"
valhalla_build_config \
  --mjolnir-tile-dir   "$VALHALLA_DIR/tiles" \
  --mjolnir-admin      "$VALHALLA_DIR/admin.sqlite" \
  --mjolnir-timezone   "$VALHALLA_DIR/tz_world.sqlite" \
  > "$CFG"

# valhalla_build_config injects Docker-default /data/valhalla/ paths for optional
# features (tile extract, traffic, landmarks, transit, elevation) that we don't
# have.  Remove them so valhalla_service falls back cleanly to tile_dir.
jq 'del(
      .mjolnir.tile_extract,
      .mjolnir.traffic_extract,
      .mjolnir.landmarks,
      .mjolnir.transit_dir,
      .mjolnir.transit_feeds_dir,
      .additional_data.elevation
    )' "$CFG" > "$CFG.tmp" && mv "$CFG.tmp" "$CFG"

echo "[setup-valhalla] Building tiles from $(basename "$CONFORMANCE_PBF")…"
valhalla_build_tiles -c "$CFG" "$CONFORMANCE_PBF"

echo "[setup-valhalla] Done — tiles at $VALHALLA_DIR/tiles"
echo "Start the service with: nix run .#start-valhalla"
