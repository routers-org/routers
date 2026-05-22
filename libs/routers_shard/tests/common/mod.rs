//! A synthetic in-memory [`ShardSource`] used by the network tests so they
//! don't have to drag in OSM-specific machinery (or PBF files) just to
//! exercise the generic builder.

use core::num::NonZeroU8;
use geo::Point;
use routers_codec::osm::OsmEdgeMetadata;
use routers_codec::osm::OsmEntryId;
use routers_shard::source::{NodeRecord, ShardSource, WayRecord};

#[derive(Debug, Clone)]
pub struct MemSource {
    pub nodes: Vec<NodeRecord<OsmEntryId>>,
    pub ways: Vec<WayRecord<OsmEntryId, OsmEdgeMetadata>>,
}

impl MemSource {
    /// Builds a synthetic grid of `cols × rows` nodes spaced `step` degrees
    /// apart, with `origin` as the SW corner. Every adjacent pair (in row
    /// order) is connected by a single bidirectional way. The resulting
    /// network is intentionally small and predictable.
    pub fn grid(origin: Point, cols: u32, rows: u32, step: f64) -> Self {
        let mut nodes = Vec::with_capacity((cols * rows) as usize);
        let mut ways = Vec::new();
        let mut next_node_id: i64 = 1;
        let mut next_way_id: i64 = 1_000_000;

        let index = |x: u32, y: u32| -> OsmEntryId { OsmEntryId::node(1 + (y * cols + x) as i64) };

        for y in 0..rows {
            for x in 0..cols {
                let pos = Point::new(origin.x() + step * x as f64, origin.y() + step * y as f64);
                nodes.push(NodeRecord {
                    id: OsmEntryId::node(next_node_id),
                    position: pos,
                });
                next_node_id += 1;
            }
        }

        let meta = OsmEdgeMetadata {
            lane_count: NonZeroU8::new(1),
            speed_limit: None,
            access: vec![],
            road_class: None, // unused on the shard side; only weight matters
        };

        // Horizontal edges
        for y in 0..rows {
            for x in 0..(cols - 1) {
                ways.push(WayRecord {
                    id: OsmEntryId::way(next_way_id),
                    refs: vec![index(x, y), index(x + 1, y)],
                    metadata: meta.clone(),
                    weight: 1,
                    bidirectional: true,
                });
                next_way_id += 1;
            }
        }
        // Vertical edges
        for y in 0..(rows - 1) {
            for x in 0..cols {
                ways.push(WayRecord {
                    id: OsmEntryId::way(next_way_id),
                    refs: vec![index(x, y), index(x, y + 1)],
                    metadata: meta.clone(),
                    weight: 1,
                    bidirectional: true,
                });
                next_way_id += 1;
            }
        }

        Self { nodes, ways }
    }
}

#[derive(Debug)]
pub struct MemSourceError;

impl ShardSource for MemSource {
    type Entry = OsmEntryId;
    type Metadata = OsmEdgeMetadata;
    type Error = MemSourceError;

    fn for_each_node<F>(&self, mut f: F) -> Result<(), Self::Error>
    where
        F: FnMut(NodeRecord<Self::Entry>) + Send,
    {
        for n in &self.nodes {
            f(*n);
        }
        Ok(())
    }

    fn for_each_way<F>(&self, mut f: F) -> Result<(), Self::Error>
    where
        F: FnMut(WayRecord<Self::Entry, Self::Metadata>) + Send,
    {
        for w in &self.ways {
            f(w.clone());
        }
        Ok(())
    }
}
