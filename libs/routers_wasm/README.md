# routers_wasm

A **WebAssembly Component** for the Routers engine. A consumer loads map
**shards** on demand and matches trajectories, routes between points, and runs
proximity queries over whatever is resident — in a browser, Node, or any
Wasmtime host, with no network or filesystem access at query time. Loading and
evicting shards keeps memory bounded while you pan a map.

## Interface

The contract is a WIT interface, [`wit/world.wit`](wit/world.wit), whose records
mirror `routers.model.v1` from the proto schema. Consumed as a component, every
host gets typed bindings — no hand-written marshalling.

```wit
resource engine {
  constructor();                                                     // empty engine
  load-shard:      func(id: string, bytes: list<u8>) -> result<_, error>;
  unload-shard:    func(id: string);
  loaded-shards:   func() -> list<string>;
  shard-of:        static func(point: coordinate) -> string;        // what a viewport needs
  shard-neighbours: static func(id: string) -> result<list<string>, error>;

  match:         func(trace: list<coordinate>, options: match-options) -> result<matched-route, error>;
  route:         func(start: coordinate, end: coordinate) -> result<route, error>;
  nearest:       func(point: coordinate) -> result<coordinate, error>;
  snap:          func(point: coordinate, radius: f64) -> result<coordinate, error>;
  nearest-edges: func(point: coordinate, radius: f64) -> result<list<edge>, error>;
}
```

Queries run over the **composite** of resident shards (`MultiShardNetwork`, a
unified `Network`), so matching and routing stitch across shard boundaries. A
whole small region is simply one shard.

First-class **npm** package published as
[`@routers-org/router`](https://www.npmjs.com/package/@routers-org/router), transpiled via
[`jco`](https://github.com/bytecodealliance/jco).

```sh
pnpm add @routers-org/router
```

```js
import { Engine } from "@routers-org/router";
const engine = new Engine();
const at = (lng, lat) => ({ latitude: lat, longitude: lng });

// As the viewport moves, load what it covers and evict the rest:
function recenter(center) {
  const id = Engine.shardOf(center);
  const needed = new Set([id, ...Engine.shardNeighbours(id)]);
  for (const s of needed) if (!resident.has(s)) engine.loadShard(s, await fetchShard(s));
  for (const s of resident)  if (!needed.has(s)) engine.unloadShard(s);
}

engine.match(trace, { searchDistance: 50, reachDistance: 5000, transport: { tag: "car", val: { height: 2, width: 1.8 } } });
engine.route(start, end);
```

**Native host** (Wasmtime) — see [`examples/wasmtime_host.rs`](examples/wasmtime_host.rs).

**Timezones** ship separately as
[`@routers-org/timezone`](../routers_tz_wasm/README.md).

## Shards

Split a network into shard blobs with the producer (geohash cells; the precision
must match `engine::SHARD_PRECISION`):

```sh
cargo run -p routers_shard --features osm,cli --bin generate-shards -- \
  --pbf region.osm.pbf --precision 6 --output shards/
```

Each `{geohash}.shard.rt` is a self-contained, filesystem-free blob. The driver
computes covering ids with `shard-of`/`shard-neighbours`, `fetch()`es them, and
calls `load-shard`; `unload-shard` frees them. Serving these behind a CDN lets a
client stream only the regions it visits.

## Build & run (E2E)

The devShell provides the `wasm32-wasip2` toolchain, `wasm-tools`, `wasmtime`,
`nodejs`, and `pnpm` (jco runs via `pnpm dlx`). From the repo root:

```sh
just wasm-build          # → a component (wasm32-wasip2)
just wasm-opt            # → the component shrunk with wasm-opt -Oz (via `jco opt`)
just wasm-transpile      # → typed ES modules in dist/transpiled (the npm payload)
just wasm-npm            # → the @routers-org/router tarball, smoke-tested
just wasm-e2e-node       # generate Sydney shards, transpile, simulate navigation + match in Node
just wasm-e2e-wasmtime   # load the shard set from a native Wasmtime host + match
just wasm-e2e            # both
```

On release, CI runs the same pipeline: the optimised component is attached to
the GitHub release, and `@routers-org/router` is published to npm at the crate's
version (bump `routers_wasm` to publish).

Single-threaded on wasm (rayon can't spawn a pool there), so
`routers_transition`/`routers_codec`/`routers_shard` are pulled with `parallel`
off.

## Layout

- `src/engine.rs` — the core: shard load/unload + `match`/`route`/`nearest`/`snap`/`nearest-edges` over the composite (host-tested with a cross-shard match: `cargo test -p routers_wasm`).
- `src/bindings.rs` — the WIT guest mapping the interface onto the core.
- `wit/world.wit` — the interface definition.
- `package.json` — the `@routers-org/router` npm package (ships `dist/transpiled`).
- `js/smoke.mjs` — the publish gate: instantiate the transpiled module, query it.
- `js/e2e.mjs` — the navigation E2E: load/evict shards while matching.

## Status

- Cross-shard matching verified natively (loads the shards a fixture trip
  crosses, matches across them, evicts).
- Compiles to `wasm32-wasip2`; both consumers exercised via `just wasm-e2e`.
- `route-element` carries geometry, edge ids, `length`, and `lane-count`; speed
  limits and road names need the trip costing runtime and are a follow-up.
