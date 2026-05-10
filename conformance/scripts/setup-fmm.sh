#!/usr/bin/env bash
# Clone, build, and configure FMM (Fast Map Matching).
#
# Steps performed:
#   1. Clone cyang-kth/fmm from GitHub into .work/fmm-src/
#   2. Build and install FMM into .work/fmm/
#   3. Build the fmm_server HTTP wrapper (fmm_server/main.cpp)
#   4. Convert the PBF road network to Shapefile (via osmium + ogr2ogr)
#   5. Generate the UBODT precomputation table
#
# All outputs land in .work/ and are fully reproducible by re-running this script.
#
# Environment variables (all have defaults):
#   CONFORMANCE_PBF         path to the .osm.pbf file
#   CONFORMANCE_WORK        writable work directory (default: $PWD/.work)
#   FMM_UBODT_MAX_DIST      upper-bound distance for UBODT generation in metres (default: 3000)
#   FMM_REPO                git repository URL for FMM (default: official GitHub)
#   FMM_REVISION            git ref to check out (default: master)
set -euo pipefail

_CONFORM_DIR="${CONFORMANCE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
CONFORMANCE_PBF="${CONFORMANCE_PBF:-$_CONFORM_DIR/../libs/routers_fixtures/resources/los-angeles-minified.osm.pbf}"
CONFORMANCE_WORK="${CONFORMANCE_WORK:-$_CONFORM_DIR/.work}"
FMM_UBODT_MAX_DIST="${FMM_UBODT_MAX_DIST:-1000}"
FMM_REPO="${FMM_REPO:-https://github.com/cyang-kth/fmm.git}"
FMM_REVISION="${FMM_REVISION:-master}"

FMM_SRC="$CONFORMANCE_WORK/fmm-src"
FMM_PREFIX="$CONFORMANCE_WORK/fmm"
FMM_SERVER_BUILD="${FMM_SERVER_BUILD:-$_CONFORM_DIR/external/fmm_server/build}"
NETWORK_DIR="$CONFORMANCE_WORK/network"
FMM_NETWORK="$NETWORK_DIR/roads.shp"
FMM_UBODT="$NETWORK_DIR/ubodt.csv"

if [ ! -f "$CONFORMANCE_PBF" ]; then
  echo "Error: PBF not found at $CONFORMANCE_PBF"
  echo "Set CONFORMANCE_PBF to point at your .osm.pbf file."
  exit 1
fi

# ── 1. Clone FMM ────────────────────────────────────────────────────────────
if [ -d "$FMM_SRC/.git" ]; then
  echo "[setup-fmm] FMM source already cloned — skipping clone."
else
  echo "[setup-fmm] Cloning FMM from $FMM_REPO ($FMM_REVISION)…"
  git clone --depth 1 --branch "$FMM_REVISION" "$FMM_REPO" "$FMM_SRC" 2>/dev/null || \
  git clone --depth 1 "$FMM_REPO" "$FMM_SRC"
fi

# ── 1.5. Patch FMM cmake ─────────────────────────────────────────────────────
# Each patch is independently idempotent.

# Patch A: upstream forces C++11; Boost 1.87+ requires C++14.  Use C++17.
if grep -qF "set(CMAKE_CXX_STANDARD 11)" "$FMM_SRC/CMakeLists.txt" 2>/dev/null; then
  echo "[setup-fmm] Patching FMM cmake: C++11 → C++17…"
  awk '/^set\(CMAKE_CXX_STANDARD 11\)/{print "set(CMAKE_CXX_STANDARD 17)"; next}1' \
    "$FMM_SRC/CMakeLists.txt" > "$FMM_SRC/CMakeLists.txt.patched"
  mv "$FMM_SRC/CMakeLists.txt.patched" "$FMM_SRC/CMakeLists.txt"
  rm -rf "$FMM_SRC/build"
fi

# Patch B: GDAL 3.3+ made OGRLayer::GetSpatialRef() return const*.
# FMM's network.cpp assigns it to a non-const pointer.
if grep -q "OGRSpatialReference \*ogrsr = ogrFDefn" "$FMM_SRC/src/network/network.cpp" 2>/dev/null; then
  echo "[setup-fmm] Patching FMM network.cpp for GDAL 3.3+ const API…"
  awk '/OGRSpatialReference \*ogrsr = ogrFDefn/{
    sub(/OGRSpatialReference \*ogrsr/, "const OGRSpatialReference *ogrsr")
  }1' "$FMM_SRC/src/network/network.cpp" > "$FMM_SRC/src/network/network.cpp.patched"
  mv "$FMM_SRC/src/network/network.cpp.patched" "$FMM_SRC/src/network/network.cpp"
fi

# Patch C: upstream unconditionally adds python/ (requires SWIG).
# Wrap in if(PYTHON_BINDING) so -DPYTHON_BINDING=OFF skips it.
if ! grep -qF "if(PYTHON_BINDING)" "$FMM_SRC/CMakeLists.txt" 2>/dev/null; then
  echo "[setup-fmm] Patching FMM cmake: make Python bindings optional…"
  awk '
    /^message\(STATUS "Add python cmake information"\)/ {
      print "if(PYTHON_BINDING)"
      print $0
      next
    }
    /^add_subdirectory\(python\)/ {
      print $0
      print "endif()"
      next
    }
    { print }
  ' "$FMM_SRC/CMakeLists.txt" > "$FMM_SRC/CMakeLists.txt.patched"
  mv "$FMM_SRC/CMakeLists.txt.patched" "$FMM_SRC/CMakeLists.txt"
  rm -rf "$FMM_SRC/build"
fi

# ── 2. Build and install FMM ────────────────────────────────────────────────
# Detect the GDAL prefix unconditionally — needed for both the FMM build and
# the fmm_server build (step 3) when FMM is already installed.
GDAL_PREFIX="$(gdal-config --prefix 2>/dev/null || true)"
if [ -n "$GDAL_PREFIX" ]; then
  CMAKE_PREFIX_PATH="${CMAKE_PREFIX_PATH:+$CMAKE_PREFIX_PATH;}$GDAL_PREFIX"
fi

if [ -f "$FMM_PREFIX/bin/ubodt_gen" ] && [ -f "$FMM_PREFIX/include/fmm/network/network.hpp" ]; then
  echo "[setup-fmm] FMM already built — skipping build."
else
  echo "[setup-fmm] Building FMM (this takes a few minutes)…"
  # Assemble OpenMP cmake hints for Apple clang (libomp from nix or system).
  OMP_ARGS=()
  if [ -n "${OMP_LIBRARY:-}" ] && [ -n "${OMP_INCLUDE:-}" ]; then
    OMP_ARGS=(
      "-DOpenMP_C_FLAGS=-Xpreprocessor -fopenmp"
      "-DOpenMP_CXX_FLAGS=-Xpreprocessor -fopenmp"
      "-DOpenMP_C_LIB_NAMES=omp"
      "-DOpenMP_CXX_LIB_NAMES=omp"
      "-DOpenMP_omp_LIBRARY=$OMP_LIBRARY"
      "-DOpenMP_C_INCLUDE_DIR=$OMP_INCLUDE"
      "-DOpenMP_CXX_INCLUDE_DIR=$OMP_INCLUDE"
      # cmake's OpenMP target doesn't always propagate omp.h's directory to
      # source-level #include search; add it to the global compile flags too.
      "-DCMAKE_CXX_FLAGS=-Xpreprocessor -fopenmp -I$OMP_INCLUDE"
      "-DCMAKE_C_FLAGS=-Xpreprocessor -fopenmp -I$OMP_INCLUDE"
    )
  fi

  cmake \
    -S "$FMM_SRC" \
    -B "$FMM_SRC/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_CXX_STANDARD=17 \
    -DCMAKE_INSTALL_PREFIX="$FMM_PREFIX" \
    ${CMAKE_PREFIX_PATH:+-DCMAKE_PREFIX_PATH="$CMAKE_PREFIX_PATH"} \
    "${OMP_ARGS[@]}" \
    -DPYTHON_BINDING=OFF \
    -DFMM_INSTALL_HEADER=ON \
    -DCMAKE_EXPORT_COMPILE_COMMANDS=OFF
  cmake --build "$FMM_SRC/build" --parallel "$(sysctl -n hw.ncpu 2>/dev/null || nproc)"
  cmake --install "$FMM_SRC/build"
  echo "[setup-fmm] FMM installed to $FMM_PREFIX"
fi

# ── 3. Build fmm_server ─────────────────────────────────────────────────────
if [ -f "$FMM_SERVER_BUILD/fmm_server" ]; then
  echo "[setup-fmm] fmm_server already built — skipping."
else
  echo "[setup-fmm] Building fmm_server HTTP wrapper…"
  cmake \
    -S "$_CONFORM_DIR/external/fmm_server" \
    -B "$FMM_SERVER_BUILD" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_PREFIX_PATH="$FMM_PREFIX${CMAKE_PREFIX_PATH:+;$CMAKE_PREFIX_PATH}"
  cmake --build "$FMM_SERVER_BUILD" --parallel "$(sysctl -n hw.ncpu 2>/dev/null || nproc)"
  echo "[setup-fmm] fmm_server built at $FMM_SERVER_BUILD/fmm_server"
fi

# ── 4. Convert PBF → Shapefile ──────────────────────────────────────────────
mkdir -p "$NETWORK_DIR"

if [ -f "$FMM_NETWORK" ]; then
  echo "[setup-fmm] Shapefile already exists — skipping conversion."
else
  echo "[setup-fmm] Filtering car-routable highway ways from PBF…"
  HIGHWAYS_PBF="$NETWORK_DIR/highways.pbf"
  # Restrict to car-driveable road types; excludes footways, cycleways, paths,
  # steps, tracks etc. which would bloat the network and slow UBODT generation.
  osmium tags-filter \
    "$CONFORMANCE_PBF" \
    "w/highway=motorway,motorway_link,trunk,trunk_link,primary,primary_link,secondary,secondary_link,tertiary,tertiary_link,residential,unclassified,service" \
    -o "$HIGHWAYS_PBF" \
    --overwrite

  echo "[setup-fmm] Exporting highway linestrings to GeoJSON…"
  GEOJSON_TMP="$NETWORK_DIR/roads_tmp.geojson"
  osmium export \
    --geometry-types=linestring \
    --output-format=geojson \
    "$HIGHWAYS_PBF" \
    -o "$GEOJSON_TMP" \
    --overwrite

  echo "[setup-fmm] Building topology shapefile (id/source/target)…"
  GEOJSON_IN="$GEOJSON_TMP" SHP_OUT="$FMM_NETWORK" \
    python3 "$_CONFORM_DIR/scripts/pbf_to_shp.py"

  rm -f "$HIGHWAYS_PBF" "$GEOJSON_TMP"
  echo "[setup-fmm] Shapefile written to $FMM_NETWORK"
fi

# ── 5. Generate UBODT ───────────────────────────────────────────────────────
if [ -f "$FMM_UBODT" ]; then
  echo "[setup-fmm] UBODT already exists — skipping generation."
else
  echo "[setup-fmm] Generating UBODT (max_dist=${FMM_UBODT_MAX_DIST}m) — this may take a while…"
  # Note: on the full LA PBF (~300×170 km) this can take several hours.
  # To speed it up, crop the PBF first or increase FMM_UBODT_MAX_DIST downwards.
  "$FMM_PREFIX/bin/ubodt_gen" \
    --network  "$FMM_NETWORK" \
    --output   "$FMM_UBODT" \
    --delta    "$FMM_UBODT_MAX_DIST" \
    --use_omp
  echo "[setup-fmm] UBODT written to $FMM_UBODT"
fi

echo ""
echo "[setup-fmm] Setup complete."
echo "Start the server with: nix run .#start-fmm"
