# Changelog

All notable changes to this project will be documented in this file.

## [0.2.1] - 2026-07-22

### ⚙️ Miscellaneous Tasks

- Updated the following local packages: routers_shard



## [0.1.1] - 2026-07-17

### 🚀 Features

- *(realtime)* Write-up naive-method matcher binary
- *(transition)* Port testing suite into transition crate
- *(transition)* Complete overhaul-refactor, written to support the trellis data structure
- *(transition)* Include examples of batch and streaming matches

### 🐛 Bug Fixes

- *(infra)* Include a bringup script
- *(realtime)* Use window-based orchestration
- *(transition)* Separate generic network trait bound from transition costing heuristic
- *(transition)* Remove network (& metadata) bounds from `CostingStrategies`
- *(transition)* Simplify arguments to context
- *(transition)* Remove layer width, and simplify resolution method supply using builder-like semantics
- *(transition)* Simplify solver-side usage, cleanup transition context naming
- *(transition)* Correct route interpolation
- *(transition)* Re-write examples
- *(trellis)* Look over and review PR
- *(transition)* Convert `generate` trait-function to use LayerId
- *(transition)* Re-write the Matcher doc comment
- *(transition)* Remove the finish(..) function, in favour of solve/snapshot
- Re-document the entire crate, needs another round of review
- Round two!
- Round three - winner!
- *(routers)* Update imports for the transition crate
- *(routers)* Imports within benchmarks
- *(transition)* Imports, compilation issues, ..


