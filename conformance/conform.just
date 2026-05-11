# Conformance recipes — run as `just conform::<matcher>` or `just conform::all`
#
# Each recipe checks that required setup has been run and offers to run it
# automatically.  Service-backed matchers (Valhalla, FMM) spin up their daemon
# for the duration of the run and shut it down on exit.

_traces_sentinel      := justfile_directory() / "data/traces/ventura.geojson"
_valhalla_sentinel    := justfile_directory() / ".work/valhalla/config.json"
_graphhopper_sentinel := justfile_directory() / ".work/graphhopper/graph-cache/properties.txt"
_fmm_network          := justfile_directory() / ".work/network/roads.shp"
_fmm_server           := justfile_directory() / "external/fmm_server/build/fmm_server"

# ── Setup guards ──────────────────────────────────────────────────────────────
# Each _ensure-* recipe checks for required files/binaries and offers to run
# the relevant setup step automatically if anything is missing.

_ensure-init:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{_traces_sentinel}}" ]; then
        echo "Missing: GPS trace files (data/traces/)"
        printf "Run 'just setup::init' now? [y/N] "
        read -r ans < /dev/tty
        case "$ans" in
            [yY]*) just --justfile "{{justfile()}}" setup::init ;;
            *) echo "Aborting."; exit 1 ;;
        esac
    fi

_ensure-valhalla:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{_valhalla_sentinel}}" ]; then
        echo "Missing: Valhalla routing tiles (.work/valhalla/)"
        printf "Run 'just setup::valhalla' now? [y/N] "
        read -r ans < /dev/tty
        case "$ans" in
            [yY]*) just --justfile "{{justfile()}}" setup::valhalla ;;
            *) echo "Aborting."; exit 1 ;;
        esac
    fi

# Soft check — GraphHopper imports the PBF automatically on first start, so
# a missing graph is not fatal.  Prompt to pre-build to avoid a ~1 min delay.
_ensure-graphhopper:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{_graphhopper_sentinel}}" ]; then
        echo "Note: GraphHopper graph not pre-built (.work/graphhopper/graph-cache/)"
        echo "  GraphHopper will import the PBF on first start (~1 min extra wait)."
        printf "Pre-build now with 'just setup::graphhopper'? [y/N] "
        read -r ans < /dev/tty
        case "$ans" in
            [yY]*) just --justfile "{{justfile()}}" setup::graphhopper ;;
            *) echo "Continuing — graph will be built on first start." ;;
        esac
    fi

_ensure-fmm:
    #!/usr/bin/env bash
    set -euo pipefail
    MISSING=()
    [ ! -f "{{_fmm_network}}" ] && MISSING+=("road network shapefile (.work/network/roads.shp)")
    [ ! -f "{{_fmm_server}}" ] && MISSING+=("fmm_server binary (external/fmm_server/build/fmm_server)")
    if [ "${#MISSING[@]}" -gt 0 ]; then
        echo "FMM setup incomplete. Missing:"
        for item in "${MISSING[@]}"; do printf "  • %s\n" "$item"; done
        printf "Run 'just setup::fmm' now? [y/N] "
        read -r ans < /dev/tty
        case "$ans" in
            [yY]*) just --justfile "{{justfile()}}" setup::fmm ;;
            *) echo "Aborting."; exit 1 ;;
        esac
    fi

# Run routers against all enabled external matchers (Valhalla + FMM)
all: _ensure-init _ensure-valhalla _ensure-fmm
    cargo run --release -p routers-conformance -- configs/all.toml

# Run routers vs Valhalla — starts and stops the service automatically.
valhalla: _ensure-init _ensure-valhalla
    #!/usr/bin/env bash
    set -euo pipefail
    PORT="${VALHALLA_PORT:-8002}"
    LOG="/tmp/valhalla-conform.log"

    echo "[conform] Starting Valhalla on port ${PORT}..."
    nix run "{{justfile_directory()}}#start-valhalla" >"$LOG" 2>&1 &
    VH_PID=$!
    trap "echo '[conform] Stopping Valhalla...'; kill $VH_PID 2>/dev/null; wait $VH_PID 2>/dev/null || true" EXIT

    echo "[conform] Waiting for Valhalla to be ready (up to 30s)..."
    for i in $(seq 1 30); do
        if curl -sf "http://localhost:$PORT/status" >/dev/null 2>&1; then
            echo "[conform] Valhalla ready."
            break
        fi
        if ! kill -0 "$VH_PID" 2>/dev/null; then
            echo "[conform] Valhalla exited unexpectedly — see $LOG"
            cat "$LOG" >&2
            exit 1
        fi
        sleep 1
    done
    if ! curl -sf "http://localhost:$PORT/status" >/dev/null 2>&1; then
        echo "[conform] Valhalla did not respond within 30s — see $LOG"
        cat "$LOG" >&2
        exit 1
    fi

    cargo run --release -p routers-conformance -- configs/valhalla.toml

# Run routers vs FMM — starts and stops the service automatically.
fmm: _ensure-init _ensure-fmm
    #!/usr/bin/env bash
    set -euo pipefail
    PORT="${FMM_PORT:-9090}"
    LOG="/tmp/fmm-conform.log"

    echo "[conform] Starting FMM server on port ${PORT}..."
    nix run "{{justfile_directory()}}#start-fmm" >"$LOG" 2>&1 &
    FMM_PID=$!
    trap "echo '[conform] Stopping FMM...'; kill $FMM_PID 2>/dev/null; wait $FMM_PID 2>/dev/null || true" EXIT

    echo "[conform] Waiting for FMM to be ready (up to 30s)..."
    for i in $(seq 1 30); do
        if curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1; then
            echo "[conform] FMM ready."
            break
        fi
        if ! kill -0 "$FMM_PID" 2>/dev/null; then
            echo "[conform] FMM exited unexpectedly — see $LOG"
            cat "$LOG" >&2
            exit 1
        fi
        sleep 1
    done
    if ! curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1; then
        echo "[conform] FMM did not respond within 30s — see $LOG"
        cat "$LOG" >&2
        exit 1
    fi

    cargo run --release -p routers-conformance -- configs/fmm.toml

# Run routers vs GraphHopper — starts and stops the service automatically.
graphhopper: _ensure-init _ensure-graphhopper
    #!/usr/bin/env bash
    set -euo pipefail
    PORT="${GH_PORT:-8989}"
    LOG="/tmp/gh-conform.log"

    echo "[conform] Starting GraphHopper on port ${PORT}..."
    nix run "{{justfile_directory()}}#start-graphhopper" >"$LOG" 2>&1 &
    GH_PID=$!
    trap "echo '[conform] Stopping GraphHopper...'; kill $GH_PID 2>/dev/null; wait $GH_PID 2>/dev/null || true" EXIT

    # GH imports the PBF on first start — allow up to 5 minutes.
    echo "[conform] Waiting for GraphHopper to be ready (up to 5 min if first run)..."
    for i in $(seq 1 60); do
        if curl -sf "http://localhost:${PORT}/info" >/dev/null 2>&1; then
            echo "[conform] GraphHopper ready."
            break
        fi
        if ! kill -0 "$GH_PID" 2>/dev/null; then
            echo "[conform] GraphHopper exited unexpectedly — see $LOG"
            cat "$LOG" >&2
            exit 1
        fi
        sleep 5
    done
    if ! curl -sf "http://localhost:${PORT}/info" >/dev/null 2>&1; then
        echo "[conform] GraphHopper did not respond within 5 min — see $LOG"
        cat "$LOG" >&2
        exit 1
    fi

    cargo run --release -p routers-conformance -- configs/graphhopper.toml
