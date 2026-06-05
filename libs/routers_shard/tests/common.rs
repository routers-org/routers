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
    /// Build a `cols × rows` grid whose south-west corner is at `origin` and
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

        for row in 0..rows {
            for col in 0..cols {
                let from = OsmEntryId::node((row * cols + col + 1) as i64);
                if col + 1 < cols {
                    let to = OsmEntryId::node((row * cols + col + 2) as i64);
                    edges.push((from, to, 1000, OsmEdgeMetadata::default()));
                    edges.push((to, from, 1000, OsmEdgeMetadata::default()));
                }
                if row + 1 < rows {
                    let to = OsmEntryId::node(((row + 1) * cols + col + 1) as i64);
                    edges.push((from, to, 1000, OsmEdgeMetadata::default()));
                    edges.push((to, from, 1000, OsmEdgeMetadata::default()));
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
