use crate::TimezoneResolver;
use geo::Rect;
use geo_index::rtree::RTreeIndex;
use routers_tz_types::storage::rtree::RTreeStorageBackend;
use routers_tz_types::timezone::ResolvedTimezones;
use std::fmt::Debug;

pub struct RTreeStorage {
    backend: &'static RTreeStorageBackend,
}

impl Debug for RTreeStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RTreeStorage")
    }
}

impl RTreeStorage {
    pub fn new() -> Self {
        RTreeStorage {
            backend: routers_tz_build::rtree::storage(),
        }
    }
}

impl TimezoneResolver for RTreeStorage {
    type Error = ();

    fn search(&self, rect: &Rect) -> Result<ResolvedTimezones, Self::Error> {
        let cache_hits = self.backend.tree.borrow().search_rect(rect);
        let timezones = cache_hits
            .into_iter()
            .map(|v| v as usize)
            .filter_map(|index| {
                self.backend
                    .geometries
                    .get(index)
                    .map(|geometries| (index, geometries))
            })
            .filter_map(|(index, _)| self.backend.names.get(index))
            .collect::<Vec<_>>();

        match timezones.as_slice() {
            [] => Err(()),
            [one] => Ok(ResolvedTimezones::Singular(one)),
            _ => Ok(ResolvedTimezones::Many(timezones)),
        }
    }
}
