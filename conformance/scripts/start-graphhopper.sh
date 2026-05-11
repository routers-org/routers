#!/usr/bin/env bash
# Start the GraphHopper HTTP service on port 8989.
# Generates a config from environment variables and runs `graphhopper server`.
# On first run GraphHopper imports the PBF (~30-60s for a city extract);
# subsequent starts reuse the cached graph and start almost instantly.
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

mkdir -p "$GH_DIR"

# Regenerate config each run so path changes (e.g. new PBF) are picked up.
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

echo "[graphhopper] Starting HTTP service on port ${GH_PORT}..."
if [ ! -f "$GH_GRAPH/properties.txt" ]; then
  echo "[graphhopper] No cached graph found — importing PBF (this takes ~1 min)..."
fi
exec graphhopper server "$GH_CONFIG"
