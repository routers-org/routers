# Changelog

All notable changes to this project will be documented in this file.

## [0.3.0] - 2026-11-13

### 🚀 Features

- *(tz)* [**breaking**] Return every zone where timezone polygons overlap

### 🐛 Bug Fixes

- *(tz)* Partition the globe into disjoint s2 cells
- *(tz)* Build the rtree index from disjoint lng/lat boxes

### ⚡ Performance

- *(tz)* Store s2cell zone indices as runs

### 🧪 Testing

- *(tz)* Check the partition and every backend against more places
- *(tz)* Check the rtree backend and run it in CI



## [0.1.4] - 2026-11-02

### ⚙️ Miscellaneous Tasks

- Update Cargo.toml dependencies



## [0.1.3] - 2026-10-07

### 🚀 Features

- *(trellis)* Build without std

### ⚙️ Miscellaneous Tasks

- Drop unused deps and spell core/alloc paths ahead of no_std



## [0.1.2] - 2026-06-13

### 🚀 Features

- WASM Support ([#147](https://github.com/routers-org/routers/pull/147))


# Changelog

All notable changes to this project will be documented in this file.
