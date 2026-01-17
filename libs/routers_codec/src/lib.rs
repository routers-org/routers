#![doc = include_str!("../docs/codec.md")]

extern crate alloc;

#[cfg(feature = "mimalloc")]
use mimalloc::MiMalloc;

#[cfg_attr(feature = "mimalloc", global_allocator)]
#[cfg(feature = "mimalloc")]
static GLOBAL: MiMalloc = MiMalloc;

pub mod osm;
pub mod primitive;

pub use primitive::Edge;
pub use primitive::Entry;
pub use primitive::Metadata;
pub use primitive::Node;
