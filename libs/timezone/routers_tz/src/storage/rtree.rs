use crate::TimezoneResolver;
use geo::{Contains, Rect};
use geo_index::rtree::RTreeIndex;
use routers_tz_types::storage::rtree::RTreeStorageBackend;
use routers_tz_types::timezone::{IANATimezoneName, ResolvedTimezones, Timezone};
use std::fmt::Debug;
use std::time::Instant;

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
        let cache_hits = {
            let now = Instant::now();
            let hits = self.backend.tree.borrow().search_rect(rect);

            let elapsed = now.elapsed();
            println!("[Search] Elapsed: {:.2?}", elapsed);
            hits
        };

        let timezones = cache_hits
            .iter()
            .filter_map(|index| self.backend.index.get(index));

        let mut resolved: Vec<IANATimezoneName> = vec![];

        for Timezone { iana, geometry } in timezones {
            if geometry.contains(rect) {
                resolved.push(iana.clone())
            }
        }

        match resolved.as_slice() {
            [] => Err(()),
            [one] => Ok(ResolvedTimezones::Singular(one.clone())),
            _ => Ok(ResolvedTimezones::Many(resolved)),
        }
    }
}
