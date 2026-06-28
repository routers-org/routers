# Changelog

All notable changes to this project will be documented in this file.

## [0.3.0] - 2026-06-28

### 🚀 Features

- *(trellis)* Initial revision of a trellis graphs structure
- *(trellis)* Use `criteron` for benchmarking to integrate with CI
- *(realtime)* Add partial historian implementation

### 🐛 Bug Fixes

- *(trellis)* Include tracing on methods and in statements
- *(trellis)* Allow data structure to be serialized
- *(*)* Add both to manifest
- *(transition)* Migrate all transition content into sub-crate
- *(root)* Re-export sub-crates under modules
- *(root)* Update imports of tests and examples
- *(realtime)* Re-write replay script to use polars as a dataframe layer
- *(realtime)* Abstract NATS into a futures-impl send/sync, use in replay
- *(realtime)* Cleanup directory, move replay into binaries
- *(realtime)* Reorganise cargo toml
- *(realtime)* Include a well-formatted progress bar
- *(realtime)* Operational historian against a cluster



## [0.2.7] - 2026-06-14

### 🚀 Features

- New Costing Functions ([#143](https://github.com/routers-org/routers/pull/143))



## [0.2.6] - 2026-06-13

### 🚀 Features

- Upgrade to `buffa/0.6.0` ([#145](https://github.com/routers-org/routers/pull/145))
- WASM Support ([#147](https://github.com/routers-org/routers/pull/147))
- Allow Compiling with `--all-features` ([#149](https://github.com/routers-org/routers/pull/149))
- Sharding Implementation ([#148](https://github.com/routers-org/routers/pull/148))
- Employ Component Model for Viewer Application ([#150](https://github.com/routers-org/routers/pull/150))

### 🐛 Bug Fixes

- Cleanup of Angular Complexity Calculation ([#142](https://github.com/routers-org/routers/pull/142))
- Consider Intra-Transitions Costs ([#141](https://github.com/routers-org/routers/pull/141))
- Double-Copying and Padded Strategy in Sharding ([#151](https://github.com/routers-org/routers/pull/151))
- Remove `tonic` Dependency ([#152](https://github.com/routers-org/routers/pull/152))



## [0.2.5] - 2026-05-18

### 🚀 Features

- *(conf.)* Basic conformance testing for valhalla, grasshopper and fmm
- *(schema)* Unified schema crate
- *(routers_grpc)* Migrated to connect-rpc
- *(routers_grpc)* Rename to routers_rpc
- *(schema)* Include formatted pbf, along with required crates for future WKT involvement
- *(timezone)* Base implementation
- *(timezone)* Type implementations for timezone
- *(timezone)* Build scripts to create implementation backends
- *(timezone)* Inline `_build` crate into `_tz` crate
- *(timezone)* Move crates into base `libs/` directory

### 💼 Other

- *(routers_grpc)* Migration to connectrpc

### 🐛 Bug Fixes

- *(schema)* Use generated connect-rpc spec
- *(routers_codec)* Update osm implementation to use `buffa`
- *(timezone)* Iterator for s2 implementation
- *(timezone)* Prefer postcard to bincode (no longer maintained)
- *(timezone)* Dependencies provided by workspace
- *(timezone)* Assign schema crate a version
- *(autorelease)* Rename schema crate to routers_schema
- *(autorelease)* Rename schema crate to routers_schema
- *(autorelease)* Force-include package contents
- *(routers_tz)* Use a pre-build strategy, only ship the prebuilt indexes



## [0.2.4] - 2026-05-05

### 🚀 Features

- Allow Saving to File ([#109](https://github.com/routers-org/routers/pull/109))
- Realtime Map-Matching via. RabbitMQ ([#113](https://github.com/routers-org/routers/pull/113))
- Visualisation Utility ([#111](https://github.com/routers-org/routers/pull/111))

### 🐛 Bug Fixes

- Initial and Final Accuracies ([#98](https://github.com/routers-org/routers/pull/98))


## [0.2.3] - 2026-03-11

### 🐛 Bug Fixes

- Multiple Fixes to Route-Base Costing Heuristics ([#97](https://github.com/routers-org/routers/pull/97))


## [0.2.2] - 2026-03-01

### 🚀 Features

- *(routers)* Add enumeration variant for solver
- *(opts)* Prefer param as map-matching options
- *(routers)* Constructor for candidates using open (unlocked) graph, utility to create square bounding box on graph
- *(generator)* Reduce search complexity by performing r* intersection over linear recollection for reduced contention
- *(generator)* Describe generator using trait, allow as a plugin
- *(snapshot)* Install `insta` dependency
- *(snapshot)* Include test for specific route, assert snapshot is equivalent from debug
- *(test)* Use insta::ron, require serde on structs
- Updated README
- *(network)* Create new crate, move common primitives and traits into it
- Impl for edges

### 🔐 Security

- *(dependencies)* Updates relevant dependencies to free crate of two vuln.

### 💼 Other

- *(ingest)* Try using contention-less metadata ingestion pattern
- *(ingest)* Preserve Fx hashing pattern
- Dump changes
- Fixing more type resolutiosn
- Isolated trait definitions and with-knowledge based implementation strategy
- Fix up some easy imports
- Getting there...
- Knock off some more bugs
- Fix solver
- Finalize as to work without req.
- Keep intersecting query
- Update snapshot

### 🐛 Bug Fixes

- *(lint)* Update to 1.88 clippy lints
- *(locking)* Do not hold lock whilst calculating
- *(routers)* Provide simplified match trait, with *_simple methods, using sensible defaults
- *(routers)* Provide helper method, Metadata::default_runtime()
- *(routers)* Clear clippy warnings, provide explicit lifetimes
- *(routers)* Update dependencies
- *(routers)* Make appropriate changes to grpc build step
- *(distace)* Use existing search distance
- *(types)* Describe locked graph from graph itself, importing from candidate
- *(types)* Describe locked graph from graph itself, importing from candidate
- *(types)* Utilities to dump geometries to a file for debugging
- *(gitignore)* Ignore specific fixtures: wkt, geojson
- *(distance)* Reduce to 50 meters
- *(dump)* Naming
- *(inst)* Rename file, specify `.snap` only
- *(inst)* Consistent sorting over metadata collections
- *(lint)* Remove leaked-in code from cherrypicked branch
- *(snap)* Update snapshot
- *(snap)* Use redactions to ease float comparison
- *(snap)* Perform recursive case
- *(snap)* Replace all x and y values
- *(codec)* Update to use network definitions
- *(routers)* Update base crate to point to network definitions
- *(grpc)* Update grpc bindings to use network definitions
- Use shared cache to improve benchmarking perf
- Use generics to allow monomorphising the network trait
- As iterators to take adv. of parallelism
- Preserve cache

### ⚙️ Miscellaneous Tasks

- Bump codspeed to v3


## [0.2.1] - 2025-06-24

### 🚀 Features

- *(structure)* Restructure routers to split responsibility into individual traits and separate concrete graph implementation
- *(bench)* Benchmarks verified against edges, edge vec implementation and initial sdk buildout
- *(match)* Remove cache from match trait, implementation-specific (i.e. on graph struct.)
- *(proto)* Split into route segment, add generic entry to services and abstract match/snap common functionality
- *(api)* Translate internal structure to protobuf repr
- *(grpc)* Add builder to sdk and types, move pick method to metadata trait and simplify service translation
- *(solver)* Add optional precomute: solver slower but easier to verify

### 💼 Other

- *(node)* Abstract map protoc. over codec::Entry impl
- *(proto)* Re-define edge information
- *(trait)* Rename Scan to Proximity
- *(api)* Decide on verb-service and verb-trait nomenclature
- *(model)* Working toward new internal routing response model
- *(metadata)* Add metadata trait into relevant definitions and structures
- *(transition)* Remove unecessary cases, add stub runtime
- *(direction)* Split into owning filter operations
- *(cache)* Gather inputs, no run
- *(cache)* Just return true
- *(pr)* Add benches for each solver, add invariant warning in cache

### 🐛 Bug Fixes

- *(routers)* Re-organise imports
- *(tests)* Update parameters to use osm by default
- *(osm)* Do not risk regression behaviour
- *(codec)* Simplify export path for osm entry id
- *(simpl)* Simplify path definitions, docs and remove Arc<..> wrapper
- *(transform)* Add transformer from collapsed to routed path
- *(cfg)* Simplify restriction patterns and take into account edge direction
- *(filter)* Remove tmp.
- *(map-op)* Assume sorted
- *(imports)* Normalize `codec` -> `routers_codec`
- *(workflow)* Update crate and workspace versioning
- *(imports)* Move prost and types to workspace-known version

### 🧪 Testing

- *(filter)* Remove filter to probe performance regression
- *(filter)* Re-add filter with unchecked fetch

### ⚙️ General Changes

- *(bench)* Probe for flaky behaviour
- Apply to config
- *(dep)* Update deps.
- *(docs)* Fill in doc comments
- Settle on petgraph 0.8.2, restore road classes bar pedestrian
- *(direction)* Use edge dynamic direction
- *(filter)* Reintroduce functionality which didnt not impact performance
- *(cfg)* Staged configurations with adapters
- *(simplify)* Statements and rename
- *(transport-mode)* Make const lookup with bitflags
- *(transport-mode)* Reduce, reduce, reduce
- *(?)* Try running function with no side effects
- *(?)* Remove paralell
- *(?)* Verify w/ blackbox
- *(?)* Perf. known, probing retaliation
- *(cache)* Move filter up a layer to preserve successor purity
- *(cache)* Unilateral filter fn
- *(hash)* Apply at the end
- *(precomp)* Simplify functionality
- *(test)* Use release mode in tests
- *(test)* Concurrent hashmap

