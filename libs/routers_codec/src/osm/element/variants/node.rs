//! Describes a simplification of an `osm::Node`. Stripping it
//! of the context information required for changelogs, and utilising
//! only the elements required for graph routing.

use crate::osm;
use crate::osm::OsmEntryId;

use routers_network::{Entry, Node};

use core::ops::{Add, Mul};
use geo::point;

impl osm::DenseNodes {
    /// Takes an `osm::DenseNodes` structure and extracts `Node`s as an
    /// iterator from `DenseNodes` with their contextual `PrimitiveBlock`.
    ///
    /// ```rust
    ///  use routers_routers_codec::osm::element::{item::Element};
    ///  use routers_routers_codec::osm::PrimitiveBlock;
    ///  use routers_routers_codec::primitive::Node;
    ///
    ///  let block: PrimitiveBlock = unimplemented!();
    ///  if let Element::DenseNodes(nodes) = block {
    ///     let nodes = Node::from_dense(nodes, 100);
    ///     for node in nodes {
    ///         println!("Node: {}", node);
    ///     }
    ///  }
    /// ```
    #[inline]
    pub fn nodes(&self, granularity: i32) -> impl Iterator<Item = Node<OsmEntryId>> + '_ {
        // Nodes are at a granularity relative to `Nanodegree`
        let scaling_factor: f64 = (granularity as f64) * 1e-9f64;

        self.lon
            .iter()
            .map(|v| *v as f64)
            .zip(self.lat.iter().map(|v| *v as f64))
            .zip(self.id.iter())
            .fold(
                vec![],
                |mut curr: Vec<Node<OsmEntryId>>, ((lng, lat), id)| {
                    let new_node = match &curr.last() {
                        Some(prior_node) => Node::new(
                            prior_node
                                .position
                                .add(point! { x: lng, y: lat }.mul(scaling_factor)),
                            OsmEntryId::node(prior_node.id.identifier() + *id),
                        ),
                        None => Node::new(
                            point! { x: lng, y: lat }.mul(scaling_factor),
                            OsmEntryId::from(*id),
                        ),
                    };

                    curr.push(new_node);
                    curr
                },
            )
            .into_iter()
    }
}

// Concrete translator for an OSM node
impl From<&osm::Node> for Node<OsmEntryId> {
    fn from(value: &osm::Node) -> Self {
        Node {
            id: OsmEntryId::node(value.id),
            position: point! { x: value.lon as f64, y: value.lat as f64 },
        }
    }
}
