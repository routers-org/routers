use geo::Rect;
use routers_tz_types::storage::s2cell::S2StorageBackend;
use s2::cellid::CellID;
use s2::latlng::LatLng;

use crate::TimezoneResolver;
use routers_tz_types::TimeZone;
use std::fmt::Debug;

const MIN_LEVEL: u64 = 1;
const MAX_LEVEL: u64 = 13;

pub struct S2CellStorage {
    backend: &'static S2StorageBackend,
}

impl Debug for S2CellStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("S2CellStorage")
    }
}

impl Default for S2CellStorage {
    fn default() -> Self {
        S2CellStorage {
            backend: routers_tz_build::s2cell::storage(),
        }
    }
}

impl TimezoneResolver for S2CellStorage {
    type Error = ();

    fn search(&self, rect: &Rect) -> Result<Vec<TimeZone>, Self::Error> {
        let center = rect.center();
        let ll = LatLng::from_degrees(center.y, center.x);
        let leaf = CellID::from(&ll);

        let mut timezones = Vec::new();

        for level in (MIN_LEVEL..=MAX_LEVEL).rev() {
            let ancestor = leaf.parent(level);
            if let Ok(pos) = self.backend.cell_ids.binary_search(&ancestor.0) {
                let tz_idx = self.backend.tz_indices[pos] as usize;
                timezones.push(TimeZone::new(self.backend.names[tz_idx].tz()));
            }
        }

        match timezones[..] {
            [] => Err(()),
            _ => Ok(timezones),
        }
    }
}
