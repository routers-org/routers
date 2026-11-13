use crate::timezone::internal::TimeZoneName;
use serde::{Deserialize, Serialize};

/// Bits dropped from a raw S2 CellID. The partition stops at level 13, so the
/// low 34 bits (below the level-13 sentinel) are always zero.
pub const CELL_SHIFT: u32 = 34;

/// Storage backend for the S2 cell-based timezone resolver.
///
/// Cells are stored as S2 CellID values shifted right by [`CELL_SHIFT`] (level still
/// encoded via S2's sentinel bit), sorted in ascending order. The parallel `tz_indices`
/// vec maps each cell to a timezone index into `names`. Lookup walks up the S2 ancestor
/// chain from a leaf cell, doing a binary search at each level until the one stored cell is found.
#[derive(Serialize, Deserialize, Debug)]
pub struct S2StorageBackend {
    /// Shifted S2 CellID values of a partition with no overlaps, sorted ascending.
    /// Serialised as deltas, which keeps the varints to a byte or two each.
    #[serde(with = "delta")]
    pub cell_ids: Vec<u32>,
    /// Timezone index for each cell (parallel to cell_ids).
    pub tz_indices: Vec<u16>,
    /// Timezone names, indexed by values in tz_indices.
    pub names: Vec<TimeZoneName>,
}

mod delta {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(ids: &[u32], serializer: S) -> Result<S::Ok, S::Error> {
        let previous = core::iter::once(&0).chain(ids);
        serializer.collect_seq(previous.zip(ids).map(|(prev, id)| id.wrapping_sub(*prev)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u32>, D::Error> {
        let mut ids = Vec::<u32>::deserialize(deserializer)?;
        let mut sum = 0u32;
        for id in &mut ids {
            sum = sum.wrapping_add(*id);
            *id = sum;
        }
        Ok(ids)
    }
}
