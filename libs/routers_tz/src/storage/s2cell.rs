use geo::Rect;
use routers_tz_types::storage::s2cell::{CELL_SHIFT, S2StorageBackend};
use s2::cellid::CellID;
use s2::latlng::LatLng;

use crate::TimezoneResolver;
use core::fmt::Debug;
use routers_tz_types::TimeZone;

const MIN_LEVEL: u64 = 0;
const MAX_LEVEL: u64 = 13;

pub struct S2CellStorage {
    backend: &'static S2StorageBackend,
}

impl Debug for S2CellStorage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("S2CellStorage")
    }
}

impl Default for S2CellStorage {
    fn default() -> Self {
        S2CellStorage {
            backend: crate::generated::s2cell::storage(),
        }
    }
}

impl S2CellStorage {}

impl TimezoneResolver for S2CellStorage {
    type Error = ();

    fn search(&self, rect: &Rect) -> Result<Vec<TimeZone>, Self::Error> {
        let center = rect.center();
        let ll = LatLng::from_degrees(center.y, center.x);
        let leaf = CellID::from(&ll);

        // Cells partition the globe, so the first stored ancestor is the only one.
        let timezones = (MIN_LEVEL..=MAX_LEVEL)
            .rev()
            .map(|level| (leaf.parent(level).0 >> CELL_SHIFT) as u32)
            .find_map(|ancestor| self.backend.cell_ids.binary_search(&ancestor).ok())
            .and_then(|pos| crate::generated::s2cell::zone(self.backend.tz_indices[pos] as usize))
            .into_iter()
            .collect::<Vec<_>>();

        match timezones[..] {
            [] => Err(()),
            _ => Ok(timezones),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_stored_name_is_in_the_tz_database() {
        let names = &crate::generated::s2cell::storage().names;
        let missing: Vec<&str> = (names.iter())
            .filter(|name| name.try_tz().is_none())
            .map(|name| &**name)
            .collect();

        assert!(missing.is_empty(), "not in the tz database: {missing:?}");
    }
}
