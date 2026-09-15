//! The materialiser: turn committed output into the served view.
//!
//! The materialiser reads the committed-output plane and *is* the served
//! history, applying every output to a sink and persisting each before it
//! acknowledges. The output plane alone defines the record.

pub mod consumer;
pub mod memory;
pub mod sink;
pub mod valkey;

pub use consumer::{Stats, run};
pub use memory::MemorySink;
pub use sink::{
    Applied, SegmentState, Sink, StoredLayer, VehicleMaterialized, merge, target_segment,
};
pub use valkey::{ValkeyError, ValkeySink};
