# Conformance recipes — run as `just conform::<matcher>` or `just conform::all`
#
# Each recipe checks that the required setup has been run and offers to run it
# automatically before proceeding.  External services must be started separately
# (just start-valhalla / just start-fmm).

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

# Run routers vs Valhalla (start-valhalla must be running)
valhalla: _ensure-init _ensure-valhalla
    cargo run --release -p routers-conformance -- configs/valhalla.toml

# Run routers vs FMM (start-fmm must be running)
fmm: _ensure-init _ensure-fmm
    cargo run --release -p routers-conformance -- configs/fmm.toml

# Run routers vs GraphHopper (requires Docker — see external/docker/docker-compose.yml)
graphhopper: _ensure-init
    cargo run --release -p routers-conformance -- configs/graphhopper.toml
