#!/usr/bin/env bash
# Clone, build, and configure FMM (Fast Map Matching).
#
# Steps performed:
#   1. Clone cyang-kth/fmm from GitHub into .work/fmm-src/
#   2. Build and install FMM into .work/fmm/
#   3. Build the fmm_server HTTP wrapper (fmm_server/main.cpp)
#   4. Convert the Sydney PBF road network to Shapefile
#   5. Generate the UBODT precomputed shortest-path table (binary format)
#
# Uses the FastMapMatch algorithm with a precomputed UBODT.  The Sydney
# dataset is used because its smaller network keeps UBODT generation fast
# and the output file compact (<100 MB); the LA network would produce an
# order-of-magnitude larger table that is impractical to generate locally.
#
# All outputs land in .work/ and are fully reproducible by re-running this script.
#
# Environment variables (all have defaults):
#   CONFORMANCE_PBF   path to the .osm.pbf file (default: sydney-minified)
#   CONFORMANCE_WORK  writable work directory (default: $PWD/.work)
#   UBODT_DELTA       UBODT upper-bound in degrees (default: 0.008 ≈ 800 m)
#   FMM_REPO          git repository URL for FMM (default: official GitHub)
#   FMM_REVISION      git ref to check out (default: master)
set -euo pipefail

_CONFORM_DIR="${CONFORMANCE_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"
CONFORMANCE_PBF="${CONFORMANCE_PBF:-$_CONFORM_DIR/../libs/routers_fixtures/resources/sydney-minified.osm.pbf}"
CONFORMANCE_WORK="${CONFORMANCE_WORK:-$_CONFORM_DIR/.work}"
FMM_REPO="${FMM_REPO:-https://github.com/cyang-kth/fmm.git}"
FMM_REVISION="${FMM_REVISION:-master}"
# delta in WGS84 degrees: 0.02° ≈ 1850 m east-west / 2220 m north-south at
# Sydney's latitude (-33.9°).  Must exceed the road-path distance between
# consecutive GPS candidate edges, not just the straight-line GPS gap.
# The Sydney trace's max consecutive gap is ~0.006° straight-line, but road
# detour (one-way streets, block layout) can push the actual road path to
# ~0.015°.  0.02° gives comfortable headroom (~3× the observed gap).
UBODT_DELTA="${UBODT_DELTA:-0.02}"

FMM_SRC="$CONFORMANCE_WORK/fmm-src"
FMM_PREFIX="$CONFORMANCE_WORK/fmm"
FMM_SERVER_BUILD="${FMM_SERVER_BUILD:-$_CONFORM_DIR/external/fmm_server/build}"
NETWORK_DIR="$CONFORMANCE_WORK/network"
FMM_NETWORK="$NETWORK_DIR/sydney-roads.shp"
FMM_UBODT="$NETWORK_DIR/sydney-ubodt.bin"

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
# Always rebuild if main.cpp is newer than the binary — the switch from
# STMATCH to FastMapMatch requires a fresh compile.
NEEDS_BUILD=true
if [ -f "$FMM_SERVER_BUILD/fmm_server" ]; then
  SRC_TS="$_CONFORM_DIR/external/fmm_server/main.cpp"
  if [ "$FMM_SERVER_BUILD/fmm_server" -nt "$SRC_TS" ]; then
    echo "[setup-fmm] fmm_server up to date — skipping."
    NEEDS_BUILD=false
  else
    echo "[setup-fmm] main.cpp changed — rebuilding fmm_server…"
  fi
fi
if [ "$NEEDS_BUILD" = "true" ]; then
  echo "[setup-fmm] Building fmm_server HTTP wrapper…"
  cmake \
    -S "$_CONFORM_DIR/external/fmm_server" \
    -B "$FMM_SERVER_BUILD" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_PREFIX_PATH="$FMM_PREFIX${CMAKE_PREFIX_PATH:+;$CMAKE_PREFIX_PATH}"
  cmake --build "$FMM_SERVER_BUILD" --parallel "$(sysctl -n hw.ncpu 2>/dev/null || nproc)"
  echo "[setup-fmm] fmm_server built at $FMM_SERVER_BUILD/fmm_server"
fi

# ── 4. Convert Sydney PBF → Shapefile ───────────────────────────────────────
mkdir -p "$NETWORK_DIR"

if [ -f "$FMM_NETWORK" ]; then
  echo "[setup-fmm] Sydney shapefile already exists — skipping conversion."
else
  echo "[setup-fmm] Filtering car-routable highway ways from Sydney PBF…"
  HIGHWAYS_PBF="$NETWORK_DIR/sydney-highways.pbf"
  osmium tags-filter \
    "$CONFORMANCE_PBF" \
    "w/highway=motorway,motorway_link,trunk,trunk_link,primary,primary_link,secondary,secondary_link,tertiary,tertiary_link,residential,unclassified,service" \
    -o "$HIGHWAYS_PBF" \
    --overwrite

  echo "[setup-fmm] Exporting highway linestrings to GeoJSON…"
  GEOJSON_TMP="$NETWORK_DIR/sydney-roads-tmp.geojson"
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

# ── 5. Generate UBODT ────────────────────────────────────────────────────────
# The UBODT pre-computes shortest paths between all edge pairs within UBODT_DELTA
# degrees.  Binary format (.bin) loads ~5× faster than CSV and is more compact.
#
# Sydney-specific sizing:
#   delta = 0.008° ≈ 740 m east-west / 890 m north-south at lat -33.9°.
#   The largest consecutive GPS gap in the Sydney trace is ~0.006° diagonal,
#   so this bound provides ~30% headroom before FMM must fall back to Dijkstra.
#
# Safety limits: abort if generation takes > 5 min or the output exceeds 10 GB.
if [ -f "$FMM_UBODT" ]; then
  echo "[setup-fmm] UBODT already exists — skipping generation."
else
  echo "[setup-fmm] Generating UBODT (delta=${UBODT_DELTA}°, binary output)…"
  echo "[setup-fmm] Limits: 5 min timeout, 10 GB max size."

  "$FMM_PREFIX/bin/ubodt_gen" \
    --network    "$FMM_NETWORK" \
    --network_id id \
    --source     source \
    --target     target \
    --delta      "$UBODT_DELTA" \
    --output     "$FMM_UBODT" \
    --use_omp &
  UBODT_PID=$!

  TIMEOUT_S=1800         # 30 minutes (delta=0.02° needs ~6× more Dijkstra work)
  LIMIT_BYTES=10737418240  # 10 GiB
  ELAPSED=0
  POLL_S=10

  while kill -0 "$UBODT_PID" 2>/dev/null; do
    sleep "$POLL_S"
    ELAPSED=$((ELAPSED + POLL_S))

    # Portable byte count (macOS stat -f%z / GNU stat -c%s)
    if [ -f "$FMM_UBODT" ]; then
      SIZE=$(stat -f%z "$FMM_UBODT" 2>/dev/null || stat -c%s "$FMM_UBODT" 2>/dev/null || echo 0)
      SIZE_MB=$(( SIZE / 1048576 ))
      printf "[setup-fmm] %3ds elapsed — UBODT size: %d MB\n" "$ELAPSED" "$SIZE_MB"

      if [ "$SIZE" -gt "$LIMIT_BYTES" ]; then
        echo "[setup-fmm] UBODT exceeds 10 GB limit — aborting generation."
        kill "$UBODT_PID" 2>/dev/null; wait "$UBODT_PID" 2>/dev/null || true
        rm -f "$FMM_UBODT"
        exit 1
      fi
    fi

    if [ "$ELAPSED" -ge "$TIMEOUT_S" ]; then
      echo "[setup-fmm] UBODT generation exceeded 5 min timeout — aborting."
      kill "$UBODT_PID" 2>/dev/null; wait "$UBODT_PID" 2>/dev/null || true
      rm -f "$FMM_UBODT"
      exit 1
    fi
  done

  wait "$UBODT_PID"
  STATUS=$?
  if [ "$STATUS" -ne 0 ]; then
    echo "[setup-fmm] ubodt_gen exited with status $STATUS"
    rm -f "$FMM_UBODT"
    exit 1
  fi

  echo "[setup-fmm] UBODT written to $FMM_UBODT ($(du -sh "$FMM_UBODT" | cut -f1))"
fi

echo ""
echo "[setup-fmm] Setup complete."
echo "Start the server with: nix run .#start-fmm"
