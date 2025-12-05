# Changelog

All notable changes to this project will be documented in this file.

## [0.1.1] - 2025-12-05

### 🚀 Features

- *(routers)* Add enumeration variant for solver
- *(routers_grpc)* Propagate search distance from api to match opts
- *(generator)* Describe generator using trait, allow as a plugin

### 🐛 Bug Fixes

- *(lint)* Update to 1.88 clippy lints
- *(rpc)* Reorders point rpcs in scan service
- *(routers)* Make appropriate changes to grpc build step
- *(distace)* Use existing search distance
- *(snap)* Replace all x and y values

RoutersOrg - 2025

## [0.1.0] - 2025-06-24

### 🚀 Features

- *(server)* Re-enable tracing, rename to `grpc` as it is more descriptive
- *(codec)* Enable tests for member crate
- *(structure)* Restructure routers to split responsibility into individual traits and separate concrete graph implementation
- *(bench)* Benchmarks verified against edges, edge vec implementation and initial sdk buildout
- *(match)* Remove cache from match trait, implementation-specific (i.e. on graph struct.)
- *(proto)* Split into route segment, add generic entry to services and abstract match/snap common functionality
- *(api)* Translate internal structure to protobuf repr
- *(grpc)* Add builder to sdk and types, move pick method to metadata trait and simplify service translation
- *(config)* Add more options to the runtime config
- *(solver)* Add optional precomute: solver slower but easier to verify

### 💼 Other

- *(deps)* Require no dangling dependencies
- *(node)* Abstract map protoc. over codec::Entry impl
- *(proto)* Re-define edge information
- *(trait)* Rename Scan to Proximity
- *(api)* Decide on verb-service and verb-trait nomenclature
- *(proto)* Final sweep
- *(model)* Working toward new internal routing response model
- *(metadata)* Add metadata trait into relevant definitions and structures

### 🐛 Bug Fixes

- *(server)* Update paths
- *(tiles)* Implement required functionality for operational server example
- *(tiles)* Allow publishing by using fqn for fixture crate
- *(routers)* Local path dep
- *(clippy)* Clippy lints on benchmarks
- *(proto)* Format proto files
- *(codec)* Simplify export path for osm entry id
- *(simpl)* Simplify path definitions, docs and remove Arc<..> wrapper
- *(docs)* Document and format
- *(srv)* Provide ctx to make filter runtime-passable
- *(imports)* Normalize `codec` -> `routers_codec`
- *(imports)* Move prost and types to workspace-known version

### 📚 Documentation

- *(proto)* Match service rpcs

### ⚙️ General Changes

- *(primitives)* Require From<&M> to elide dsb
- *(access)* Derive accessablility checks
- *(proto)* Re-define costing heuristics
- *(cfg)* Staged configurations with adapters

RoutersOrg - 2025
