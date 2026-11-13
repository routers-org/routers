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

        let timezones = (MIN_LEVEL..=MAX_LEVEL)
            .rev()
            .map(|level| (leaf.parent(level).0 >> CELL_SHIFT) as u32)
            .find_map(|ancestor| self.backend.cell_ids.binary_search(&ancestor).ok())
            .map(|pos| self.backend.zones(pos))
            .unwrap_or_default()
            .iter()
            .filter_map(|&zone| crate::generated::s2cell::zone(zone as usize))
            .collect::<Vec<_>>();

        match timezones[..] {
            [] => Err(()),
            _ => Ok(timezones),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CELL_SHIFT, MAX_LEVEL};
    use s2::cellid::CellID;

    #[test]
    fn stored_cells_are_disjoint_and_name_known_zones() {
        let storage = crate::generated::s2cell::storage();
        let cells: Vec<CellID> = (storage.cell_ids.iter())
            .map(|&id| CellID(u64::from(id) << CELL_SHIFT))
            .collect();

        assert_eq!(cells.len(), storage.tz_indices.len());
        assert!((cells.iter()).all(|cell| cell.is_valid() && cell.level() <= MAX_LEVEL));
        assert!(
            cells
                .windows(2)
                .all(|w| w[0].range_max() < w[1].range_min())
        );

        let indices = storage.names.len() + storage.overlaps.len();
        assert!((storage.tz_indices.iter()).all(|&index| (index as usize) < indices));
        assert!((storage.overlaps.iter()).all(|zones| {
            zones.len() > 1
                && zones.windows(2).all(|w| w[0] < w[1])
                && (zones.iter()).all(|&zone| (zone as usize) < storage.names.len())
        }));
    }

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
