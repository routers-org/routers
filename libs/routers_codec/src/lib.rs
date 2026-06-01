#![doc = include_str!("../docs/codec.md")]

extern crate alloc;

#[cfg(all(feature = "mimalloc", not(target_arch = "wasm32")))]
use mimalloc::MiMalloc;

#[cfg(all(feature = "mimalloc", not(target_arch = "wasm32")))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

pub mod osm;
pub mod primitive;
