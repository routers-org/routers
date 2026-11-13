use geo::Rect;
use geo_index::rtree::RTreeIndex;
use routers_tz_types::storage::rtree::RTreeStorageBackend;

use crate::TimezoneResolver;
use core::fmt::Debug;
use routers_tz_types::TimeZone;

pub struct RTreeStorage {
    backend: &'static RTreeStorageBackend,
}

impl Debug for RTreeStorage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RTreeStorage")
    }
}

impl Default for RTreeStorage {
    fn default() -> Self {
        RTreeStorage {
            backend: crate::generated::rtree::storage(),
        }
    }
}

impl TimezoneResolver for RTreeStorage {
    type Error = ();

    fn search(&self, rect: &Rect) -> Result<Vec<TimeZone>, Self::Error> {
        let (x, y) = (rect.center().x as f32, rect.center().y as f32);
        let timezones = (self.backend.tree.borrow_this())
            .search(x, y, x, y)
            .first()
            .map(|&item| self.backend.zones(item as usize))
            .unwrap_or_default()
            .iter()
            .filter_map(|&zone| crate::generated::rtree::zone(zone as usize))
            .collect::<Vec<TimeZone>>();

        match timezones[..] {
            [] => Err(()),
            _ => Ok(timezones),
        }
    }
}

#[cfg(test)]
mod tests {
    use geo_index::rtree::RTreeIndex;

    #[test]
    fn stored_boxes_are_disjoint_and_name_known_zones() {
        let storage = crate::generated::rtree::storage();
        let tree = storage.tree.borrow_this();
        assert_eq!(tree.num_items() as usize, storage.zones.len());

        let (zones, overlaps) = (storage.names.len(), &storage.overlaps);
        super::super::assert_valid_indices(&storage.zones, zones, overlaps);

        let leaves = tree.boxes_at_level(0).expect("tree has leaves");
        for b in leaves.chunks_exact(4) {
            let (x, y) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
            assert_eq!(
                tree.search(x, y, x, y).len(),
                1,
                "boxes overlap at ({x}, {y})"
            );
        }
    }
}
