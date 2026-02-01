#![doc = include_str!("../docs/codec.md")]

extern crate alloc;

#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

#[cfg_attr(feature = "mimalloc", global_allocator)]
#[cfg(feature = "mimalloc")]
static GLOBAL: MiMalloc = MiMalloc;

pub mod osm;
pub mod primitive;
