use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::DirectionAwareEdgeId;
use routers_shard::{ShardSource, SourceEdge};

/// Synthetic grid data source for tests.
///
/// Creates a rectangular grid of nodes connected by bidirectional edges to
/// their horizontal and vertical neighbours. Node ids start at 1 and increase
/// row-major: node(row * cols + col + 1).
pub struct MemSource {
    nodes: Vec<(OsmEntryId, Point)>,
    edges: Vec<SourceEdge<OsmEntryId, OsmEdgeMetadata>>,
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

        // Each two-way way carries a distinct-ish lane count so tests can
        // tell which way's metadata a shard kept.
        let mut lanes = (1u8..=u8::MAX).cycle();
        let mut ways = 0;
        let mut road = |from, to| {
            ways += 1;
            let id = DirectionAwareEdgeId::new(OsmEntryId::way(ways));
            let meta = OsmEdgeMetadata {
                lane_count: core::num::NonZeroU8::new(lanes.next().unwrap()),
                ..OsmEdgeMetadata::default()
            };
            [
                (from, to, (1000, id.forward()), meta.clone()),
                (to, from, (1000, id.backward()), meta),
            ]
        };

        for row in 0..rows {
            for col in 0..cols {
                let from = OsmEntryId::node((row * cols + col + 1) as i64);
                if col + 1 < cols {
                    let to = OsmEntryId::node((row * cols + col + 2) as i64);
                    edges.extend(road(from, to));
                }
                if row + 1 < rows {
                    let to = OsmEntryId::node(((row + 1) * cols + col + 1) as i64);
                    edges.extend(road(from, to));
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
    ) -> Box<dyn Iterator<Item = SourceEdge<OsmEntryId, OsmEdgeMetadata>> + 'a> {
        Box::new(self.edges.iter().cloned())
    }
}
