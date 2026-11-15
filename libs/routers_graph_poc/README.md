# routers_graph_poc

A proof of concept: the routing network as flat arrays instead of hash maps.
Not wired into the matcher — it exists to measure what the layout is worth.

`OsmNetwork` keeps topology in a hash-keyed `DiGraphMap`, positions in a
second hash map and way metadata in a third. `CompactGraph` holds the same
network as columns: dense `u32` node indices numbered along a Z-order curve,
positions as two `f64` columns, and CSR adjacency with each edge's length and
road class stored beside its target.

`Search` is the matcher's bounded Dijkstra over that layout. It reproduces the
existing cost model and tie-breaking exactly, so it settles the same nodes
with the same parents (`tests/parity.rs` asserts this), but works against
generation-stamped arrays and allocates nothing once warm.

## Measuring

```sh
POC_REGION=sydney cargo run --release -p routers_graph_poc --example compare
POC_REGION=sydney cargo run --release -p routers_graph_poc --example compare --features mimalloc
```

Los Angeles is the default region, but ingesting it needs more than 3.4 GiB of
memory with either graph.

## Results (Sydney extract, 678 k nodes, 1.22 M edges, no LTO, aarch64)

| | `OsmNetwork` | `CompactGraph` |
|---|---|---|
| Ingest from PBF, with edge r-tree | 436 ms | 211 ms |
| Load from saved bytes | 367 ms | 6 ms (r-tree not included: +65 ms) |
| Saved size | 38 MiB | 40 MiB |
| Bounded search, 1 km reach, cold | 39 µs | 12.6 µs |
| Bounded search, 4 km reach, cold | 656 µs | 287 µs |
| Point-to-point route | 35.2 ms | 7.3 ms |

The topology columns are 39 MiB. Searches and route costs are identical.

Allocator: under mimalloc the existing 1 km search slowed to 68 µs while the
compact one stayed at 12.8 µs; nothing here got faster with it.

## As a `Network`

`CompactNetwork` implements `DataPlane`, `Discovery`, `Scan` and `Route` with
`OsmNetwork`'s own entry, metadata and runtime types, so it goes wherever the
matcher takes a network. It also answers `DataPlane::reach`, the hook through
which a network runs the matcher's bounded search over its own storage.

```sh
POC_REGION=sydney cargo run --release -p routers_graph_poc --example match_compare
```

Sydney trip (15 points, 5 km reach, thin LTO), same route from both:

| | `OsmNetwork` | `CompactNetwork` |
|---|---|---|
| Cold match, 12 threads | 57 ms | 12 ms |
| Cold match, 1 thread | 224 ms | 85 ms |
| Warm match, 12 threads | 0.41 ms | 0.38 ms |
| Warm match, 1 thread | 0.94 ms | 0.96 ms |

A warm match never searches, and everything else it asks of a network goes
through the same id-keyed trait methods, so the two are level there.

## What it does not do

- The matcher still identifies everything by OSM id, so warm matches gain
  nothing; that needs candidates and trees to carry graph indices.
- No SIMD. Exact Haversine has no vectorised form on stable Rust; the columns
  are laid out so that an approximate distance could be batched.
- The id-to-node lookup is a hash map built on demand, not a stored column.
- Ingest still holds every PBF node before discarding those no road touches.
