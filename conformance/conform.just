# Conformance recipes — run as `just conform::<matcher>` or `just conform::all`
#
# Each recipe checks that required setup has been run and offers to run it
# automatically.  Service-backed matchers (Valhalla, FMM) spin up their daemon
# for the duration of the run and shut it down on exit.

_traces_sentinel   := justfile_directory() / "data/traces/ventura.geojson"
_valhalla_sentinel := justfile_directory() / ".work/valhalla/config.json"
_fmm_sentinel      := justfile_directory() / ".work/network/ubodt.csv"

# Ensure trace files exist, prompt to run setup::init if not.
_ensure-init:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{_traces_sentinel}}" ]; then
        printf "Trace files have not been initialised. Run 'just setup::init' now? [y/N] "
        read -r ans < /dev/tty
        case "$ans" in
            [yY]*) just --justfile "{{justfile()}}" setup::init ;;
            *) echo "Aborting."; exit 1 ;;
        esac
    fi

# Ensure Valhalla tiles exist, prompt to run setup::valhalla if not.
_ensure-valhalla:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{_valhalla_sentinel}}" ]; then
        printf "Valhalla has not been set up. Run 'just setup::valhalla' now? [y/N] "
        read -r ans < /dev/tty
        case "$ans" in
            [yY]*) just --justfile "{{justfile()}}" setup::valhalla ;;
            *) echo "Aborting."; exit 1 ;;
        esac
    fi

# Ensure FMM network and UBODT exist, prompt to run setup::fmm if not.
_ensure-fmm:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{_fmm_sentinel}}" ]; then
        printf "FMM has not been set up. Run 'just setup::fmm' now? [y/N] "
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

# Run routers vs GraphHopper (requires Docker — see external/docker/docker-compose.yml)
graphhopper: _ensure-init
    cargo run --release -p routers-conformance -- configs/graphhopper.toml
