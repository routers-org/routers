# Changelog

All notable changes to this project will be documented in this file.

## [routers_codec-v0.1.0] - 2025-06-24

### 🚀 Features

- *(restr.)* Corrected fixture locations using crate-based directory resolution
- *(codec)* Enable tests for member crate
- *(structure)* Restructure routers to split responsibility into individual traits and separate concrete graph implementation
- *(grpc)* Add builder to sdk and types, move pick method to metadata trait and simplify service translation
- *(impl)* Introduce for edge metadata
- *(config)* Add more options to the runtime config
- *(solver)* Add optional precomute: solver slower but easier to verify

### 💼 Other

- Update dependencies and structures so imports resolve
- *(deps)* Require no dangling dependencies
- *(node)* Abstract map protoc. over codec::Entry impl
- *(metadata)* Add metadata trait into relevant definitions and structures
- *(speed-limit)* Parser structure
- *(speed-limit)* Stabilise structures and formalise solution tests
- *(primitives)* Removing dsb
- *(probe)* Verify perf. due to higher searchable zone
- *(transition)* Remove unecessary cases, add stub runtime
- *(direction)* Split into owning filter operations
- *(cache)* Gather inputs, no run
- *(cache)* Just return true
- *(cache)* Try non-functional approach

### 🐛 Bug Fixes

- Builds, includes, proto fmt
- *(codec)* Use mimalloc
- *(benchmark)* Resolve clippy warnings in benchmarks
- *(tests)* Repair testing framework
- *(clippy)* Resolve lint warnings
- *(tiles)* Implement required functionality for operational server example
- *(tiles)* Allow publishing by using fqn for fixture crate
- *(tiles)* Use local path
- *(tiles)* Give it a readme
- *(routers)* Update imports and make corresponding modifications
- *(readme)* Urls
- *(osm)* Do not risk regression behaviour
- *(osm)* Clippy lints, enable member type in debug but keep disabled for release perf.
- *(clippy)* Clippy lints on benchmarks
- *(proto)* Format proto files
- *(codec)* Simplify export path for osm entry id
- *(docs)* Document and format
- *(clippy)* Resolve lint failures
- *(clippy)* Convert to Display impl for Condition
- *(tests)* Resolve failing cases
- *(srv)* Provide ctx to make filter runtime-passable
- *(tests)* Correct test invariant
- *(osm)* Restore pedestrian road class
- *(cfg)* Simplify restriction patterns and take into account edge direction
- *(filter)* Remove tmp.
- *(filter)* Repr directionality as u8
- *(filter)* Remove tmp.
- *(filter)* Remove other properties to probe benchmarker
- *(map-op)* Assume sorted
- *(imports)* Normalize `codec` -> `routers_codec`
- *(imports)* Move prost and types to workspace-known version

### 🧪 Testing

- *(filter)* Without sort, const restr
- *(filter)* Reorder filters

### ⚙️ General Changes

- Update benchmark structure
- *(codec)* Fix lints
- *(codec)* Move `osm` into separate module to isolate import
- *(codec)* Resolve clippy warnings
- *(split)* Split as terminator
- *(tests)* Add furthers tests
- *(tests)* Test transport and direction
- *(primitives)* Require From<&M> to elide dsb
- *(modules)* Reorganise modules
- *(modules)* Isolate access tag and road classification, preferring the classified road type in graph creation & weighting
- *(inline)* Inline and const weighting fn
- *(dep)* Update deps.
- *(docs)* Fill in doc comments
- Settle on petgraph 0.8.2, restore road classes bar pedestrian
- *(access)* Derive accessablility checks
- *(cond)* Flip conditions
- *(filter)* Represent as u8; bitpacks restriction to 16bit
- *(filter)* Reintroduce functionality which didnt not impact performance
- *(access)* Rename from LandAccess to Access
- *(access)* Restore builder parser (not performance breaking
- *(default)* Use breaking default from 2c36a0
- *(cfg)* Staged configurations with adapters
- *(transport-mode)* Make const lookup with bitflags
- *(transport-mode)* Reduce, reduce, reduce
- *(?)* Try running function with no side effects
- *(?)* Perf. known, probing retaliation
- *(cache)* Move filter up a layer to preserve successor purity
- *(trace)* Remove trace-log

RoutersOrg - 2025
