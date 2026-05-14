# Setup recipes — run as `just setup::<target>` or `just setup::all`

# Generate trace GeoJSON files from built-in fixtures (one-time)
init:
    cargo run -p routers-conformance -- init-traces

# Build Valhalla routing tiles from the PBF (run once before start-valhalla)
valhalla:
    bash {{justfile_directory()}}/scripts/setup-valhalla.sh

# Pre-import GraphHopper graph from the PBF (optional — start-graphhopper does this automatically)
graphhopper:
    bash {{justfile_directory()}}/scripts/setup-graphhopper.sh

# Build FMM from source and prepare the road network (run once before start-fmm)
fmm:
    bash {{justfile_directory()}}/scripts/setup-fmm.sh

# Run all setup steps in order
all: init valhalla graphhopper fmm
