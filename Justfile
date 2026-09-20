mod tz "libs/routers_tz"
mod infra "infrastructure"

init VERSION="2026a":
    just tz download {{ VERSION }}

# Run benchmarks. Writes updated snapshots to .snap.new instead of panicking,
# so all scenarios complete even when heuristics change.
# Run benchmarks. Updates snapshots in place so all scenarios complete even
# when heuristics change. Review changes afterwards with `just bench-review`.
bench:
    git lfs pull --include="benches/snapshots/*" --exclude=""
    INSTA_UPDATE=always cargo bench

# Review snapshot changes after `just bench` via git diff.
bench-review:
    git diff benches/snapshots/

# === WebAssembly component (libs/routers_wasm) ===

jco := "pnpm dlx @bytecodealliance/jco@1.32"

# Build the map-matching component (needs the wasm32-wasip2 toolchain from the flake).
wasm-build:
    cargo build -p routers_wasm --target wasm32-wasip2 --release

# Optimise the WASM build using binaryen.
wasm-opt: wasm-build
    {{ jco }} opt target/wasm32-wasip2/release/routers_wasm.wasm \
      -o target/wasm32-wasip2/release/routers_wasm.opt.wasm -- -Oz

# Split the Sydney fixture into shard blobs the demos load on demand.
wasm-shards:
    git lfs pull --include="libs/routers_fixtures/resources/sydney-minified.osm.pbf"
    cargo run -q -p routers_shard --features osm,cli --bin generate-shards -- \
      --pbf libs/routers_fixtures/resources/sydney-minified.osm.pbf \
      --precision 6 --output libs/routers_wasm/dist/shards

# Transpile the optimised component to typed JS/TS.
wasm-transpile: wasm-opt
    {{ jco }} transpile target/wasm32-wasip2/release/routers_wasm.opt.wasm \
      --name routers_wasm -o libs/routers_wasm/dist/transpiled

# Stage the @routers-org/wasm npm tarball (what CI publishes on release).
wasm-npm: wasm-transpile
    cd libs/routers_wasm && pnpm install && pnpm test && pnpm pack

# E2E in Node: simulate map navigation, loading/evicting shards, then match.
wasm-e2e-node: wasm-shards wasm-transpile
    cd libs/routers_wasm && pnpm install && pnpm test
    node libs/routers_wasm/examples/js/e2e.mjs

# E2E from a native Wasmtime host: load the shard set, then match.
wasm-e2e-wasmtime: wasm-build wasm-shards
    cargo run -p routers_wasm --example wasmtime_host -- target/wasm32-wasip2/release/routers_wasm.wasm libs/routers_wasm/dist/shards

# Full E2E across both consumers.
wasm-e2e: wasm-e2e-node wasm-e2e-wasmtime

# === Service images ===

# Regenerate the protobuf schema code the crates compile against.
schema:
    buf generate

# Build the realtime images; the Dockerfiles share one cargo cache, so only the first build is cold.
build: schema
    just infra build
