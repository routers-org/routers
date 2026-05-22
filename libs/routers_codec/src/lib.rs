#![doc = include_str!("../docs/codec.md")]

extern crate alloc;

// `mimalloc` is only ever compiled on non-WASM targets (see Cargo.toml),
// so the `global_allocator` registration is gated on the same cfg to keep
// `cargo check --target wasm32-unknown-unknown --features mimalloc` from
// trying to reach a dep that simply isn't there.
#[cfg(all(feature = "mimalloc", not(target_arch = "wasm32")))]
use mimalloc::MiMalloc;

#[cfg(all(feature = "mimalloc", not(target_arch = "wasm32")))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

pub mod osm;
pub mod primitive;
