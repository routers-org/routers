# Changelog

All notable changes to this project will be documented in this file.

## [0.2.0] - 2026-11-02

### 🐛 Bug Fixes

- *(shard)* [**breaking**] Keep source edge ids on shard edges
- Upgrade scc, s2, wasmtime, tabled and codspeed for RUSTSEC advisories

### ⚙️ Miscellaneous Tasks

- *(shard)* Declare the ShardSource API break as 0.3.0
- Declare the buffa 0.8 API break in schema, codec, rpc, realtime and routers



## [0.1.2] - 2026-10-07

### 🐛 Bug Fixes

- Manual release
- *(ci)* Derive API check eligibility from Cargo metadata

### ⚙️ Miscellaneous Tasks

- *(routers)* Release v0.3.11
- Release
- Release
- Drop unused deps and spell core/alloc paths ahead of no_std



## [0.1.1] - 2026-09-04

### 🚀 Features

- *(wasm)* Shard-loaded map-matching + routing WebAssembly component
- *(wasm)* Publish @routers-org/wasm with a wasm-opt -Oz core
- *(wasm)* Flat entry point for @routers-org/wasm

### 🐛 Bug Fixes

- *(wasm)* Link WASI into the wasmtime host example
- *(deploy)* Use managed versioning workflow
- *(deploy)* Simplify docs
- Merge
- *(release)* Let release-plz version routers_wasm

### ⚙️ Miscellaneous Tasks

- Release
- *(wasm)* Trim index.js comment


