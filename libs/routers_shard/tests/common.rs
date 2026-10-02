use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::edge::Weight;
use routers_shard::ShardSource;

/// Synthetic grid data source for tests.
///
/// Creates a rectangular grid of nodes connected by bidirectional edges to
/// their horizontal and vertical neighbours. Node ids start at 1 and increase
/// row-major: node(row * cols + col + 1).
pub struct MemSource {
    nodes: Vec<(OsmEntryId, Point)>,
    edges: Vec<(OsmEntryId, OsmEntryId, Weight, OsmEdgeMetadata)>,
}

impl MemSource {
    /// Build a `cols x rows` grid whose south-west corner is at `origin` and
    /// whose cells are `step` degrees apart on each axis.
    pub fn grid(origin: Point, cols: u32, rows: u32, step: f64) -> Self {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();

        for row in 0..rows {
            for col in 0..cols {
                let id = OsmEntryId::node((row * cols + col + 1) as i64);
                let pos = Point::new(
                    origin.x() + col as f64 * step,
                    origin.y() + row as f64 * step,
                );
                nodes.push((id, pos));
            }
        }

        // Each edge carries a distinct-ish lane count so tests can tell
        // which edge's metadata a shard kept.
        let mut lanes = (1u8..=u8::MAX).cycle();
        let mut tagged = |from, to| {
            let lane_count = core::num::NonZeroU8::new(lanes.next().unwrap());
            (
                from,
                to,
                1000,
                OsmEdgeMetadata {
                    lane_count,
                    ..OsmEdgeMetadata::default()
                },
            )
        };

        for row in 0..rows {
            for col in 0..cols {
                let from = OsmEntryId::node((row * cols + col + 1) as i64);
                if col + 1 < cols {
                    let to = OsmEntryId::node((row * cols + col + 2) as i64);
                    edges.push(tagged(from, to));
                    edges.push(tagged(to, from));
                }
                if row + 1 < rows {
                    let to = OsmEntryId::node(((row + 1) * cols + col + 1) as i64);
                    edges.push(tagged(from, to));
                    edges.push(tagged(to, from));
                }
            }
        }

        Self { nodes, edges }
    }
}

impl ShardSource<OsmEntryId, OsmEdgeMetadata> for MemSource {
    fn nodes<'a>(&'a self) -> Box<dyn Iterator<Item = (OsmEntryId, Point)> + 'a> {
        Box::new(self.nodes.iter().copied())
    }

    fn edges<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = (OsmEntryId, OsmEntryId, Weight, OsmEdgeMetadata)> + 'a> {
        Box::new(self.edges.iter().cloned())
    }
}
