//! Durable orchestrator state a partition worker recovers after a crash. The
//! [`checkpoint`] prepare → publish → promote sequence keeps each commit crash-safe.

pub mod checkpoint;
pub mod valkey;
