//! Durable orchestrator state: vehicle checkpoints, prepared commits, frontiers.
//!
//! This is the orchestrator's only persistent memory. The bus holds messages in
//! flight; the store holds the committed truth a partition worker must recover
//! after a crash — each vehicle's retained `Trip`,
//! the in-flight prepared commit that survives promotion, and the partition's
//! completion frontier.
//!
//! * [`checkpoint`] — the record types and the
//!   [`CheckpointStore`](checkpoint::CheckpointStore) trait: the
//!   prepare → publish → promote sequence that makes a commit crash-safe, and
//!   the in-memory fake that unit tests drive it against.
//! * [`valkey`] — the Valkey-backed implementation. Per-vehicle keys carry a
//!   `{vehicle:<id>}` hash tag so prepare/promote stay atomic over one key group
//!   on a future Cluster; placement across primaries reuses the rendezvous hash.
//!
//! Checkpoint bytes are opaque to the trait (no `E` generic); the typed
//! [`VehicleCheckpoint`](checkpoint::VehicleCheckpoint) encodes and decodes them
//! beside the record.

pub mod checkpoint;
pub mod valkey;
