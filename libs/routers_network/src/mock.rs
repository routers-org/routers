//! A mock implementation of [`Network`] for unit-testing the map-matching
//! algorithm without requiring real OSM data.
//!
//! # Overview
//!
//! [`MockNetwork`] implements all of the traits required by the routing
//! engine (`Discovery`, `Scan`, `Route`, `Network`) using in-memory
//! data structures that are quick to populate.  A companion
//! [`MockNetworkBuilder`] provides a fluent API so tests can describe a
//! small synthetic road network in just a few lines of code.
//!
//! # Example
//!
//! ```rust
//! use routers::testing::{MockNetworkBuilder, MockEntryId, MockMetadata};
//! use geo::Point;
//!
//! // Build a tiny two-edge straight road: 1 → 2 → 3
//! let network = MockNetworkBuilder::new()
//!     .node(1, Point::new(-118.15, 34.15))
//!     .node(2, Point::new(-118.16, 34.15))
//!     .node(3, Point::new(-118.17, 34.15))
//!     .edge(1, 2)
//!     .edge(2, 3)
//!     .build();
//! ```

use core::fmt::Debug;
use core::hash::BuildHasherDefault;

use crate::{
    DataPlane, Direction, DirectionAwareEdgeId, Discovery, Edge, Entry, Metadata, Node, Route,
    Scan, edge::Weight, network::GraphEdge,
};
use geo::Point;
use petgraph::prelude::DiGraphMap;
use rstar::{AABB, RTree};
use rustc_hash::{FxHashMap, FxHasher};
use serde::Serialize;

// ── Entry ────────────────────────────────────────────────────────────────────

/// A minimal node / edge identifier for use inside a [`MockNetwork`].
///
/// Wraps an `i64` and derives all traits required by [`Entry`].
#[derive(Default, Serialize, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MockEntryId(pub i64);

impl Entry for MockEntryId {
    #[inline]
    fn identifier(&self) -> i64 {
        self.0
    }
}

// ── Metadata ─────────────────────────────────────────────────────────────────

/// Trivial metadata that considers every edge accessible in every direction.
///
/// Use this when the test only cares about topology, not access restrictions.
#[derive(Clone, Debug, Default, Serialize)]
pub struct MockMetadata;

impl Metadata for MockMetadata {
    /// There is no raw form for mock metadata; construction is always unit.
    type Raw<'a> = ();
    /// No runtime context is needed; all edges are always accessible.
    type Runtime = ();
    /// No trip context is needed.
    type TripContext = ();

    fn pick(_raw: ()) -> Self {
        MockMetadata
    }

    fn runtime(_ctx: Option<()>) -> () {}

    fn accessible(&self, _access: &(), _direction: Direction) -> bool {
        true
    }
}

// ── Network ───────────────────────────────────────────────────────────────────

type GraphStructure = DiGraphMap<
    MockEntryId,
    (Weight, DirectionAwareEdgeId<MockEntryId>),
    BuildHasherDefault<FxHasher>,
>;

/// An in-memory road network for unit tests.
///
/// Build one via [`MockNetworkBuilder`].
pub struct MockNetwork {
    graph: GraphStructure,
    nodes: FxHashMap<MockEntryId, Node<MockEntryId>>,
    metadata: FxHashMap<MockEntryId, MockMetadata>,
    node_index: RTree<Node<MockEntryId>>,
    edge_index: RTree<Edge<Node<MockEntryId>>>,
}

impl Debug for MockNetwork {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("mock network")
    }
}

impl Discovery<MockEntryId> for MockNetwork {
    fn edges_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = Edge<Node<MockEntryId>>> + Send + 'a>
    where
        MockEntryId: 'a,
    {
        Box::new(
            self.edge_index
                .locate_in_envelope_intersecting(&aabb)
                .cloned(),
        )
    }

    fn nodes_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Node<MockEntryId>> + Send + 'a>
    where
        MockEntryId: 'a,
    {
        Box::new(self.node_index.locate_in_envelope(&aabb))
    }

    fn node(&self, id: &MockEntryId) -> Option<&Node<MockEntryId>> {
        self.nodes.get(id)
    }

    fn edge(&self, source: &MockEntryId, target: &MockEntryId) -> Option<Edge<MockEntryId>> {
        self.graph
            .edge_weight(*source, *target)
            .map(|&(weight, id)| Edge {
                source: *source,
                target: *target,
                weight,
                id,
            })
    }
}

impl Scan<MockEntryId> for MockNetwork {
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<MockEntryId>>
    where
        MockEntryId: 'a,
    {
        self.node_index.nearest_neighbor(point)
    }
}

impl Route<MockEntryId> for MockNetwork {
    fn route_nodes(
        &self,
        start_node: MockEntryId,
        finish_node: MockEntryId,
    ) -> Option<(Weight, Vec<Node<MockEntryId>>)> {
        let (score, path) = petgraph::algo::astar(
            &self.graph,
            start_node,
            |finish| finish == finish_node,
            |(_, _, w)| w.0,
            |_| 0,
        )?;

        let route = path
            .iter()
            .filter_map(|v| self.nodes.get(v).copied())
            .collect();

        Some((score, route))
    }
}

impl DataPlane for MockNetwork {
    type Entry = MockEntryId;
    type Meta = MockMetadata;

    fn metadata(&self, id: &MockEntryId) -> Option<&MockMetadata> {
        self.metadata.get(id)
    }

    fn point(&self, id: &MockEntryId) -> Option<Point> {
        self.nodes.get(id).map(|n| n.position)
    }

    fn edges_outof<'a>(
        &'a self,
        id: MockEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<MockEntryId>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Outgoing)
                .map(|(src, dst, &data)| (src, dst, data)),
        )
    }

    fn edges_into<'a>(
        &'a self,
        id: MockEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<MockEntryId>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Incoming)
                .map(|(src, dst, &data)| (src, dst, data)),
        )
    }

    fn fatten(
        &self,
        Edge {
            source,
            target,
            weight,
            id,
        }: &Edge<MockEntryId>,
    ) -> Option<Edge<Node<MockEntryId>>> {
        Some(Edge {
            source: *self.nodes.get(source)?,
            target: *self.nodes.get(target)?,
            id: DirectionAwareEdgeId::new(Node::new(dummy_point(), id.index()))
                .with_direction(id.direction()),
            weight: *weight,
        })
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Edge weight used when none is explicitly specified in the builder.
const DEFAULT_WEIGHT: Weight = 1;

/// Returns a placeholder [`Point`] used as the position field of a `Node<E>` wrapper
/// inside a [`DirectionAwareEdgeId`].  The routing engine stores edge identifiers as
/// `Node<E>` purely for generic compatibility; the position field is not used in that
/// context and may be set to any fixed value.
#[inline(always)]
fn dummy_point() -> Point {
    Point::new(0., 0.)
}

/// A node definition accumulated by [`MockNetworkBuilder`].
struct NodeDef {
    id: MockEntryId,
    position: Point,
}

/// An edge definition accumulated by [`MockNetworkBuilder`].
///
/// The `edge_id` field is the identifier stored in the
/// [`DirectionAwareEdgeId`] — this is the key under which
/// [`MockMetadata`] is retrieved by the routing engine.
struct EdgeDef {
    source: MockEntryId,
    target: MockEntryId,
    weight: Weight,
    edge_id: MockEntryId,
}

/// Fluent builder for [`MockNetwork`].
///
/// # Usage
///
/// 1. Add nodes with [`node`](MockNetworkBuilder::node).
/// 2. Add directed edges with [`edge`](MockNetworkBuilder::edge) or
///    bidirectional roads with
///    [`bidirectional_edge`](MockNetworkBuilder::bidirectional_edge).
/// 3. Call [`build`](MockNetworkBuilder::build) to get the network.
///
/// Node and edge identifiers are arbitrary `i64` values chosen by
/// the caller; they only need to be unique within the network.
pub struct MockNetworkBuilder {
    nodes: Vec<NodeDef>,
    edges: Vec<EdgeDef>,
    /// Monotonically-increasing counter used to auto-assign edge IDs.
    next_edge_id: i64,
}

impl Default for MockNetworkBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MockNetworkBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            next_edge_id: 1,
        }
    }

    /// Register a node at the given geographic position.
    ///
    /// `id` must be unique within the network being built.
    pub fn node(mut self, id: i64, position: Point) -> Self {
        self.nodes.push(NodeDef {
            id: MockEntryId(id),
            position,
        });
        self
    }

    /// Add a directed edge from `source` to `target` with `DEFAULT_WEIGHT`.
    ///
    /// Both `source` and `target` must correspond to previously-added nodes.
    pub fn edge(self, source: i64, target: i64) -> Self {
        self.weighted_edge(source, target, DEFAULT_WEIGHT)
    }

    /// Add a directed edge from `source` to `target` with an explicit weight.
    pub fn weighted_edge(mut self, source: i64, target: i64, weight: Weight) -> Self {
        let edge_id = MockEntryId(self.next_edge_id);
        self.next_edge_id += 1;

        self.edges.push(EdgeDef {
            source: MockEntryId(source),
            target: MockEntryId(target),
            weight,
            edge_id,
        });
        self
    }

    /// Add two directed edges (forward and reverse) between `a` and `b`.
    ///
    /// Both directions share the same edge identifier, which mirrors how
    /// OSM bidirectional ways are stored (one way ID, two directed edges).
    pub fn bidirectional_edge(self, a: i64, b: i64) -> Self {
        self.bidirectional_weighted_edge(a, b, DEFAULT_WEIGHT)
    }

    /// Add two directed edges (forward and reverse) with an explicit weight.
    pub fn bidirectional_weighted_edge(mut self, a: i64, b: i64, weight: Weight) -> Self {
        let edge_id = MockEntryId(self.next_edge_id);
        self.next_edge_id += 1;

        self.edges.push(EdgeDef {
            source: MockEntryId(a),
            target: MockEntryId(b),
            weight,
            edge_id,
        });
        self.edges.push(EdgeDef {
            source: MockEntryId(b),
            target: MockEntryId(a),
            weight,
            edge_id,
        });
        self
    }

    /// Consume the builder and produce a [`MockNetwork`].
    pub fn build(self) -> MockNetwork {
        let mut graph = GraphStructure::new();
        let mut nodes: FxHashMap<MockEntryId, Node<MockEntryId>> = FxHashMap::default();
        let mut metadata: FxHashMap<MockEntryId, MockMetadata> = FxHashMap::default();

        for NodeDef { id, position } in &self.nodes {
            nodes.insert(*id, Node::new(*position, *id));
        }

        for EdgeDef {
            source,
            target,
            weight,
            edge_id,
        } in &self.edges
        {
            let direction_aware = DirectionAwareEdgeId::new(*edge_id);
            graph.add_edge(*source, *target, (*weight, direction_aware));

            // Every edge ID must have an entry in the metadata map so that
            // `Network::metadata` never returns `None` for a valid edge.
            metadata.entry(*edge_id).or_default();
        }

        // Build fat edges for the spatial index — only include edges where
        // both endpoints have registered node positions.
        let fat_edges: Vec<Edge<Node<MockEntryId>>> = self
            .edges
            .iter()
            .filter_map(|e| {
                let src_node = *nodes.get(&e.source)?;
                let tgt_node = *nodes.get(&e.target)?;
                let direction_aware =
                    DirectionAwareEdgeId::new(Node::new(dummy_point(), e.edge_id));
                Some(Edge {
                    source: src_node,
                    target: tgt_node,
                    weight: e.weight,
                    id: direction_aware,
                })
            })
            .collect();

        let node_index = RTree::bulk_load(nodes.values().copied().collect());
        let edge_index = RTree::bulk_load(fat_edges);

        MockNetwork {
            graph,
            nodes,
            metadata,
            node_index,
            edge_index,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use geo::point;

    /// Build the tiny straight-road network used in several tests:
    ///
    /// ```text
    ///  1 ──────── 2 ──────── 3
    /// (-118.15)  (-118.16)  (-118.17)   lat = 34.15
    /// ```
    fn straight_road() -> MockNetwork {
        MockNetworkBuilder::new()
            .node(1, point!(x: -118.15, y: 34.15))
            .node(2, point!(x: -118.16, y: 34.15))
            .node(3, point!(x: -118.17, y: 34.15))
            .edge(1, 2)
            .edge(2, 3)
            .build()
    }

    // ── Builder & Discovery ───────────────────────────────────────────────────

    #[test]
    fn builder_registers_nodes() {
        let net = straight_road();
        assert!(net.node(&MockEntryId(1)).is_some());
        assert!(net.node(&MockEntryId(2)).is_some());
        assert!(net.node(&MockEntryId(3)).is_some());
        assert!(net.node(&MockEntryId(99)).is_none());
    }

    #[test]
    fn builder_registers_edges() {
        let net = straight_road();
        assert!(net.edge(&MockEntryId(1), &MockEntryId(2)).is_some());
        assert!(net.edge(&MockEntryId(2), &MockEntryId(3)).is_some());
        // Reverse direction should not exist for a one-way edge.
        assert!(net.edge(&MockEntryId(2), &MockEntryId(1)).is_none());
    }

    #[test]
    fn bidirectional_edge_creates_both_directions() {
        let net = MockNetworkBuilder::new()
            .node(1, point!(x: -118.15, y: 34.15))
            .node(2, point!(x: -118.16, y: 34.15))
            .bidirectional_edge(1, 2)
            .build();

        assert!(net.edge(&MockEntryId(1), &MockEntryId(2)).is_some());
        assert!(net.edge(&MockEntryId(2), &MockEntryId(1)).is_some());
    }

    #[test]
    fn nearest_node_returns_closest() {
        let net = straight_road();
        // A point much closer to node 2 than to nodes 1 or 3.
        let query = point!(x: -118.161, y: 34.151);
        let nearest = net.nearest_node(&query).expect("nearest node must exist");
        assert_eq!(nearest.id, MockEntryId(2));
    }

    #[test]
    fn metadata_present_for_all_edges() {
        let net = straight_road();
        // The edge IDs are auto-assigned starting from 1.
        assert!(net.metadata(&MockEntryId(1)).is_some());
        assert!(net.metadata(&MockEntryId(2)).is_some());
    }

    #[test]
    fn mock_metadata_always_accessible() {
        let meta = MockMetadata;
        assert!(meta.accessible(&(), Direction::Outgoing));
        assert!(meta.accessible(&(), Direction::Incoming));
    }

    // ── Routing ───────────────────────────────────────────────────────────────

    #[test]
    fn route_nodes_finds_direct_path() {
        let net = straight_road();
        let (_, path) = net
            .route_nodes(MockEntryId(1), MockEntryId(3))
            .expect("route must exist");
        let ids: Vec<i64> = path.iter().map(|n| n.id.0).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn route_nodes_returns_none_for_unreachable() {
        let net = straight_road();
        // Nodes 1→3 exist but 3→1 is unreachable in a one-way network.
        assert!(net.route_nodes(MockEntryId(3), MockEntryId(1)).is_none());
    }

}
