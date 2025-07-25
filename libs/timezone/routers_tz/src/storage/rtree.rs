use crate::TimezoneResolver;
use geo::Geometry::MultiPolygon;
use geo::{Contains, ConvexHull, Rect};
use geo_index::rtree::RTreeIndex;
use geozero::ToGeo;
use geozero::wkt::Wkt;
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
        let cache_hits = self.backend.tree.borrow().search_rect(rect);
        let timezones = cache_hits
            .into_iter()
            .map(|v| v as usize)
            .filter_map(|index| {
                self.backend
                    .geometries
                    .get(index)
                    .map(|geometries| (index, geometries))
            });

        let mut resolved: Vec<IANATimezoneName> = vec![];

        for (index, geometry) in timezones {
            // if let MultiPolygon(geometry) = Wkt(geometry).to_geo().expect("failed to convert geometry to wkt") {
            //     if geometry.contains(rect) {
            let iana: &IANATimezoneName = self.backend.names.get(index).unwrap();
            resolved.push(iana.clone())
            //     }
            // }
        }

        match resolved.as_slice() {
            [] => Err(()),
            [one] => Ok(ResolvedTimezones::Singular(one.clone())),
            _ => Ok(ResolvedTimezones::Many(resolved)),
        }
    }
}
