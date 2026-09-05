# Changelog

All notable changes to this project will be documented in this file.

## [0.3.5] - 2026-09-05

### ⚙️ Miscellaneous Tasks

- Updated the following local packages: routers_shard



## [0.3.4] - 2026-09-01

### 🐛 Bug Fixes

- *(transition)* Swap the match cache to a TLS-free backing on wasm32
- *(deploy)* Simplify docs
- Merge



## [0.3.3] - 2026-08-30

### 🚀 Features

- *(transition,codec)* Gate rayon behind a parallel feature

### 🐛 Bug Fixes

- *(transition)* Use cfg-if to feature-gate
- *(transition)* Restore weigh_boundary parallel pipeline



## [0.3.2] - 2026-08-26

### ⚙️ Miscellaneous Tasks

- Updated the following local packages: routers_codec, routers_shard



## [0.3.1] - 2026-08-13

### ⚙️ Miscellaneous Tasks

- Updated the following local packages: routers_shard



## [0.3.0] - 2026-08-11

### 🚀 Features

- *(transition)* Carry observation timestamps through the trip
- *(realtime)* [**breaking**] Emit convergence diffs and cut the trip behind them
- *(realtime)* Route requests by geography and degrade foreign resumes

### 🐛 Bug Fixes

- Reduce default reach distance to 1km
- *(fixtures)* Interpolate trip fixtures for the 1km reach
- Reduce Default Reach Distance ([#231](https://github.com/routers-org/routers/pull/231))

### ⚙️ Miscellaneous Tasks

- *(deps)* Drop rstar from remaining manifests and refresh lockfile



## [0.2.2] - 2026-07-30

### 🚀 Features

- *(trait)* Use associated types for Metadata and Entry traits on Network
- *(transition)* Use bounded cache size of 10,000 elements

### 🐛 Bug Fixes

- *(transition)* Formatting, etc.



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


