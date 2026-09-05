# Changelog

All notable changes to this project will be documented in this file.

## [0.4.6] - 2026-09-05

### ⚙️ Miscellaneous Tasks

- Update Cargo.lock dependencies



## [0.4.5] - 2026-09-01

### ⚙️ Miscellaneous Tasks

- Updated the following local packages: routers_shard, routers_transition



## [0.4.4] - 2026-08-30

### 💼 Other

- Isolate heavy dependency trees from everyday builds



## [0.4.3] - 2026-08-26

### ⚙️ Miscellaneous Tasks

- Updated the following local packages: routers_codec, routers_shard, routers_transition



## [0.4.2] - 2026-08-13

### ⚙️ Miscellaneous Tasks

- Updated the following local packages: routers_shard, routers_transition



## [0.4.1] - 2026-08-13

### ⚙️ Miscellaneous Tasks

- Update Cargo.lock dependencies



## [0.4.0] - 2026-08-11

### 🚀 Features

- *(realtime)* Multiple performance and throughput-oriented improvements
- *(realtime)* [**breaking**] Widen vehicle id to u64 and retire trip id
- *(realtime)* Extract the stable hashing contract into a partition module
- *(transition)* Carry observation timestamps through the trip
- *(realtime)* [**breaking**] Emit convergence diffs and cut the trip behind them
- *(realtime)* [**breaking**] Matchers serve request/reply behind a queue group
- *(realtime)* [**breaking**] Durable partitioned ingest and the owner-authoritative orchestrator
- *(realtime)* Route requests by geography and degrade foreign resumes
- *(infra)* [**breaking**] Deploy the vehicle-partitioned fleet

### 🐛 Bug Fixes

- *(realtime)* Type enforcement



## [0.3.2] - 2026-07-30

### 🚀 Features

- *(trait)* Use associated types for Metadata and Entry traits on Network
- *(realtime)* Historian to serialise reads via. queue
- *(realtime)* Use optimise protobuf-backed event packets with integer vehicle-id and trip-id

### 🐛 Bug Fixes

- *(realtime)* Formatting



## [0.3.1] - 2026-07-22

### ⚙️ Miscellaneous Tasks

- Update Cargo.lock dependencies



## [0.2.0] - 2026-07-17

### 🚀 Features

- *(realtime)* Add partial historian implementation
- *(realtime)* Template orchestator (echo-style)
- *(realtime)* Allow orchestrator to pull from kv store
- *(realtime)* Write-up naive-method matcher binary

### 🐛 Bug Fixes

- *(realtime)* Re-write replay script to use polars as a dataframe layer
- *(realtime)* Abstract NATS into a futures-impl send/sync, use in replay
- *(realtime)* Cleanup script
- *(realtime)* Cleanup directory, move replay into binaries
- *(realtime)* Reorganise cargo toml
- *(realtime)* Include a well-formatted progress bar
- *(realtime)* Assign client name during connection
- *(realtime)* Operational historian against a cluster
- *(realtime)* Remove unused imports
- *(infra)* Include a bringup script
- *(shard)* Include a binary to generate files into the shard cache from a given sourcefile
- *(realtime)* Remove jetstream dep
- *(realtime)* Use static precision level of 4
- *(realtime)* Use window-based orchestration
- Round three - winner!



## [0.1.2] - 2026-06-14

### 🚀 Features

- *(realtime)* Historian, matcher and orchestrator binaries

### 🐛 Bug Fixes

- *(realtime)* Move into realtime crate
- *(realtime)* Justfiles on each, allow building all containers in one command
- *(infra)* Loop till done
- *(infra)* Non-busy loop



## [0.1.1] - 2026-05-18

### 🐛 Bug Fixes

- *(changelog)* Remove footer



## [0.1.0] - 2026-05-05

### 🚀 Features

- Realtime Map-Matching via. RabbitMQ ([#113](https://github.com/routers-org/routers/pull/113))

### 🐛 Bug Fixes

- *(viewer)* Convert to lib, make fixture a dev dependency
- *(viewer)* Format and clippy fixes

