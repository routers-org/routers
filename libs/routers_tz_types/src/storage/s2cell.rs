use crate::timezone::internal::TimeZoneName;
use serde::{Deserialize, Serialize};

/// Bits dropped from a raw S2 CellID. The partition stops at level 13, so the
/// low 34 bits (below the level-13 sentinel) are always zero.
pub const CELL_SHIFT: u32 = 34;

/// Storage backend for the S2 cell-based timezone resolver.
///
/// Cells are stored as S2 CellID values shifted right by [`CELL_SHIFT`] (level still
/// encoded via S2's sentinel bit), sorted in ascending order. The parallel `tz_indices`
/// vec maps each cell to its zones (see [`super::zones`]). Lookup walks up the S2 ancestor
/// chain from a leaf cell, doing a binary search at each level until the one stored cell is found.
#[derive(Serialize, Deserialize, Debug)]
pub struct S2StorageBackend {
    /// Shifted S2 CellID values of a partition with no overlaps, sorted ascending.
    /// Serialised as deltas, which keeps the varints to a byte or two each.
    #[serde(with = "delta")]
    pub cell_ids: Vec<u32>,
    /// Zone index for each cell (parallel to cell_ids).
    #[serde(with = "runs")]
    pub tz_indices: Vec<u16>,
    pub names: Vec<TimeZoneName>,
    /// Sorted zones of each place where timezone polygons overlap.
    pub overlaps: Vec<Vec<u16>>,
}

impl S2StorageBackend {
    pub fn zones(&self, pos: usize) -> &[u16] {
        super::zones(&self.tz_indices[pos], self.names.len(), &self.overlaps)
    }
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

/// Runs of one zone as varints `len << 2 | tag`: tags 0–2 reuse a recent run's zone, `ESCAPE` is followed by the zone.
mod runs {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    pub const ESCAPE: u32 = 3;

    fn touch(recent: &mut Vec<u16>, zone: u16) {
        recent.retain(|z| *z != zone);
        recent.insert(0, zone);
        recent.truncate(ESCAPE as usize + 1);
    }

    pub fn serialize<S: Serializer>(zones: &[u16], serializer: S) -> Result<S::Ok, S::Error> {
        let mut recent = Vec::new();
        let mut words = Vec::new();
        for run in zones.chunk_by(|a, b| a == b) {
            let (zone, len) = (run[0], run.len() as u32);
            match recent.iter().skip(1).position(|z| *z == zone) {
                Some(tag) => words.push(len << 2 | tag as u32),
                None => words.extend([len << 2 | ESCAPE, u32::from(zone)]),
            }
            touch(&mut recent, zone);
        }
        serializer.collect_seq(words)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u16>, D::Error> {
        let mut words = Vec::<u32>::deserialize(deserializer)?.into_iter();
        let mut recent = Vec::new();
        let mut zones = Vec::new();
        while let Some(word) = words.next() {
            let zone = match word & ESCAPE {
                ESCAPE => words
                    .next()
                    .and_then(|zone| u16::try_from(zone).ok())
                    .ok_or_else(|| D::Error::custom("run is missing its zone"))?,
                tag => *recent
                    .get(tag as usize + 1)
                    .ok_or_else(|| D::Error::custom("run reuses an unseen zone"))?,
            };
            zones.extend(core::iter::repeat_n(zone, (word >> 2) as usize));
            touch(&mut recent, zone);
        }
        Ok(zones)
    }

    #[cfg(test)]
    mod tests {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Zones(#[serde(with = "super")] Vec<u16>);

        #[test]
        fn round_trips_alternating_and_new_zones() {
            let zones = vec![7, 7, 3, 7, 7, 7, 3, 9, 3, 300, 300, 1, 9, 9, 7];
            let bytes = postcard::to_allocvec(&Zones(zones.clone())).unwrap();
            let Zones(decoded) = postcard::from_bytes(&bytes).unwrap();
            assert_eq!(decoded, zones);
        }
    }
}
