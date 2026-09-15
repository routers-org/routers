//! Load/failure harness for `routers_realtime` (T31).
//!
//! Cargo compiles each `tests/*.rs` as its own crate, so this one file is the
//! whole harness crate: it pulls in [`harness`], which holds the shared
//! [`fixture`](harness::fixture) and the scenario modules. Every scenario runs
//! in-process against the memory bus and store under paused time, so the suite
//! is deterministic and finishes in well under the five-second budget.

extern crate alloc;

#[path = "harness/mod.rs"]
mod harness;
