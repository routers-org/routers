# Changelog

All notable changes to this project will be documented in this file.

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

RoutersOrg - 2025
