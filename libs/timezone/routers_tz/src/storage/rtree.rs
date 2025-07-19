use geo::{Contains, Rect};
use geo_index::rtree::{RTreeIndex};
use routers_tz_types::storage::rtree::RTreeStorageBackend;
use routers_tz_types::timezone::{IANATimezoneName, ResolvedTimezones, Timezone};
use crate::TimezoneResolver;

pub struct RTreeStorage {
    backend: &'static RTreeStorageBackend,
}

impl RTreeStorage {
    pub fn new() -> Self {
        RTreeStorage {
            backend: routers_tz_build::rtree::STORAGE
        }
    }
}

impl TimezoneResolver for RTreeStorage {
    type Error = ();

    fn search(&self, rect: &Rect) -> Result<ResolvedTimezones, Self::Error> {
        let cache_hits = self.backend.tree.search_rect(rect);

        let mut resolved: Vec<IANATimezoneName> = vec![];

        for Timezone { iana, geometry } in cache_hits {
            if geometry.contains(rect) {
                resolved.push(iana)
            }
        }

        match resolved.as_slice() {
            [] => Err(()),
            [one] => Ok(ResolvedTimezones::Singular(one.clone())),
            _ => Ok(ResolvedTimezones::Many(resolved)),
        }
    }
}
