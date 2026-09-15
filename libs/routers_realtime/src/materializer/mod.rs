//! The materialiser: turn committed output into the served view. (T28)
//!
//! The materialiser is the one consumer that reads the committed-output plane
//! (`events.matched.v1.p.>`) and *is* the served history: it applies every
//! output — new revisions, geometry changes, retractions, finalisation — to a
//! sink, persisting each before it acknowledges, and recovering its own
//! consumption independently of the orchestrator. It never borrows the
//! orchestrator's in-memory trip cache as truth; the output plane alone defines
//! the record (spec A4, contract DESIGN.md §10).
//!
//! The pieces:
//!
//! * [`sink`] — the pure [`merge`](sink::merge) rule and the [`Sink`](sink::Sink)
//!   trait every backend implements.
//! * [`memory`] — [`MemorySink`](memory::MemorySink), the in-memory view for
//!   tests and viewer-like observers.
//! * [`valkey`] — [`ValkeySink`](valkey::ValkeySink), the Valkey-backed served
//!   view (compile-verified; there is no store in this sandbox).
//! * [`consumer`] — [`run`](consumer::run), the apply-then-acknowledge loop.

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
