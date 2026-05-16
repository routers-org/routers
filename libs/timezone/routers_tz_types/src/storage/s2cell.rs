use crate::timezone::internal::TimeZoneName;
use bincode::{Decode, Encode};

/// Storage backend for the S2 cell-based timezone resolver.
///
/// Cells are stored as raw u64 CellID values (with level encoded via S2's sentinel bit),
/// sorted in ascending order. The parallel `tz_indices` vec maps each cell to a timezone
/// index into `names`. Lookup walks up the S2 ancestor chain from a leaf cell, doing a
/// binary search at each level until a stored cell is found.
#[derive(Encode, Decode, Debug)]
pub struct S2StorageBackend {
    /// S2 CellID values (raw u64 with level sentinel) from the covering, sorted ascending.
    pub cell_ids: Vec<u64>,
    /// Timezone index for each cell (parallel to cell_ids).
    pub tz_indices: Vec<u32>,
    /// Timezone names, indexed by values in tz_indices.
    pub names: Vec<TimeZoneName>,
}
