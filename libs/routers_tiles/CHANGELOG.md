# Changelog

All notable changes to this project will be documented in this file.

## [0.1.3] - 2026-06-13

### 🚀 Features

- Allow Compiling with `--all-features` ([#149](https://github.com/routers-org/routers/pull/149))
- Sharding Implementation ([#148](https://github.com/routers-org/routers/pull/148))



## [0.1.2] - 2026-05-18

### 🚀 Features

- *(routers_grpc)* Rename to routers_rpc

### 🐛 Bug Fixes

- *(changelog)* Remove footer



## [0.1.1] - 2026-03-01

### 🐛 Bug Fixes

- *(lint)* Update to 1.88 clippy lints
- *(routers)* Update dependencies
- *(deps)* Allow alloc via extern
- *(deps)* Inherit lints from workspace
- *(deps)* Lint according to global linting rules
- *(deps)* Format code to match rules


## [0.1.0] - 2025-06-24

### 🚀 Features

- *(workspace)* Restructure server into discrete services
- *(codec)* Enable tests for member crate

### 💼 Other

- Update dependencies and structures so imports resolve

### 🐛 Bug Fixes

- *(tests)* Repair testing framework
- *(tiles)* Implement required functionality for operational server example
- *(tiles)* Fix import deps.
- *(routers)* Update imports and make corresponding modifications
- *(clippy)* Clippy lints on benchmarks
- *(proto)* Format proto files
- *(imports)* Normalize `codec` -> `routers_codec`
- *(imports)* Move prost and types to workspace-known version

### ⚙️ General Changes

- *(workflow)* Add audit
- Tracer working

