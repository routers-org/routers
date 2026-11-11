# routers_tz_wasm

A **WebAssembly Component** for the Routers timezone resolver. It answers which
IANA timezone covers a point, bounding box, or polygon — in a browser, Node, or
any Wasmtime host, with no network access. The boundary and tz database data
(~1 MiB) is bundled, so it ships apart from the routing engine
([`@routers-org/router`](../routers_wasm/README.md)).

## Interface

The contract is [`wit/world.wit`](wit/world.wit). Records mirror
`routers.model.v1` and calls mirror `api.timezone.v1.TimezoneService`:

```wit
get-from-point:              func(coordinate: coordinate, timestamp: option<timestamp>) -> result<list<timezone>, error>;
batch-get-from-points:       func(coordinates: list<coordinate>, timestamp: option<timestamp>) -> result<list<option<timezone>>, error>;
get-from-bounding-box:       func(bounding-box: bounding-box, timestamp: option<timestamp>) -> result<list<timezone>, error>;
batch-get-from-bounding-box: func(bounding-boxes: list<bounding-box>, timestamp: option<timestamp>) -> result<list<option<timezone>>, error>;
get-from-polygon:            func(polygon: polygon, timestamp: option<timestamp>) -> result<list<timezone>, error>;
batch-get-from-polygon:      func(polygons: list<polygon>, timestamp: option<timestamp>) -> result<list<option<timezone>>, error>;
```

`utc-offset` and `is-dst` are those in effect at `timestamp`, or now when it
is unset. Offsets are clock parts with the offset's sign: -03:30 is
`{ hours: -3, minutes: -30 }`.

## npm

Published as [`@routers-org/timezone`](https://www.npmjs.com/package/@routers-org/timezone),
transpiled via [`jco`](https://github.com/bytecodealliance/jco).

```js
import { getFromPoint, batchGetFromPoints } from "@routers-org/timezone";

getFromPoint({ latitude: -33.87, longitude: 151.21 });
// [{ ianaCode: "Australia/Sydney", utcOffset: { hours: 11, minutes: 0, seconds: 0 }, isDst: true }]
batchGetFromPoints(trace); // one zone (or undefined) per point

// The offset on a past date; `seconds` is a BigInt (WIT `s64`).
getFromPoint(sydney, { seconds: BigInt(Date.parse("2026-07-15") / 1000), nanos: 0 });
// [{ ianaCode: "Australia/Sydney", utcOffset: { hours: 10, ... }, isDst: false }]
```

The first call decodes the bundled data (~8 ms); later lookups take ~12 µs.

## Build

From the repo root, `just wasm-build`, `just wasm-transpile` and `just wasm-npm`
build both components. On release, CI attaches the optimised component to the
crate's GitHub release and publishes `@routers-org/timezone` at the crate's
version.

## Layout

- `src/resolver.rs` — the core: point, box and polygon lookups (host-tested: `cargo test -p routers_tz_wasm`).
- `src/bindings.rs` — the WIT guest mapping the interface onto the core.
- `wit/world.wit` — the interface definition.
- `package.json` — the `@routers-org/timezone` npm package (ships `dist/transpiled`).
- `examples/js/smoke.mjs` — the publish gate.
