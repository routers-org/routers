#!/usr/bin/env bash
# Pre-import GraphHopper routing graph from the configured PBF.
# Run once to avoid the ~1 min import delay on first `just conform::graphhopper`.
# Re-run only if the network data changes.
#
# Environment variables (all have defaults):
#   CONFORMANCE_PBF   path to the .osm.pbf file
#   CONFORMANCE_WORK  writable work directory (default: $PWD/.work)
set -euo pipefail

_CONFORM_DIR="${CONFORMANCE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
CONFORMANCE_PBF="${CONFORMANCE_PBF:-$_CONFORM_DIR/../libs/routers_fixtures/resources/los-angeles-minified.osm.pbf}"
CONFORMANCE_WORK="${CONFORMANCE_WORK:-$_CONFORM_DIR/.work}"
GH_PORT="${GH_PORT:-8989}"
GH_DIR="$CONFORMANCE_WORK/graphhopper"
GH_GRAPH="$GH_DIR/graph-cache"
GH_CONFIG="$GH_DIR/config.yml"

if [ ! -f "$CONFORMANCE_PBF" ]; then
  echo "Error: PBF not found at $CONFORMANCE_PBF"
  echo "Set CONFORMANCE_PBF to point at your .osm.pbf file."
  exit 1
fi

if [ -f "$GH_GRAPH/properties.txt" ]; then
  echo "[setup-graphhopper] Graph already imported — skipping."
  echo "Delete $GH_GRAPH to force a reimport."
  exit 0
fi

mkdir -p "$GH_DIR"

cat > "$GH_CONFIG" <<GHCFG
graphhopper:
  datareader.file: $CONFORMANCE_PBF
  graph.location: $GH_GRAPH
  profiles:
    - name: car
      custom_model_files: [car.json]
  profiles_ch:
    - profile: car
  profiles_lm: []
  graph.encoded_values: car_access, car_average_speed, road_access
  import.osm.ignored_highways: footway,construction,cycleway,path,steps
server:
  application_connectors:
    - type: http
      port: $GH_PORT
  admin_connectors:
    - type: http
      port: $((GH_PORT + 1))
GHCFG

echo "[setup-graphhopper] Importing $(basename "$CONFORMANCE_PBF") (takes ~1 min)..."
graphhopper import "$GH_CONFIG"

echo "[setup-graphhopper] Done — graph at $GH_GRAPH"
echo "Start the service with: nix run .#start-graphhopper"
