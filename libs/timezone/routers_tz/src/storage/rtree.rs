use geo::Rect;
use geo_index::rtree::RTreeIndex;
use routers_tz_types::storage::rtree::RTreeStorageBackend;

use crate::TimezoneResolver;
use routers_tz_types::TimeZone;
use std::fmt::Debug;

pub struct RTreeStorage {
    backend: &'static RTreeStorageBackend,
}

impl Debug for RTreeStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RTreeStorage")
    }
}

impl Default for RTreeStorage {
    fn default() -> Self {
        RTreeStorage {
            backend: routers_tz_build::rtree::storage(),
        }
    }
}

impl TimezoneResolver for RTreeStorage {
    type Error = ();

    fn search(&self, rect: &Rect) -> Result<TimeZone, Self::Error> {
        let cache_hits =
            self.backend
                .tree
                .borrow_this()
                .neighbors_coord(&rect.center(), Some(1), None);

        cache_hits
            .into_iter()
            .filter_map(|index| self.backend.names.get(index as usize))
            .map(|name| TimeZone::new(name.tz()))
            .next()
            .ok_or(())
    }
}
