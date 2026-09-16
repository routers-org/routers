//! Load/failure harness for `routers_realtime`: cargo compiles this `tests/*.rs`
//! as its own crate, pulling in the [`harness`] module tree (fixture + scenarios).

extern crate alloc;

#[path = "harness/mod.rs"]
mod harness;
