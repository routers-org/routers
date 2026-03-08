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

use geo::Point;
use petgraph::prelude::DiGraphMap;
use routers_network::{
    Direction, DirectionAwareEdgeId, Discovery, Edge, Entry, Metadata, Network, Node, Route, Scan,
    edge::Weight,
    network::GraphEdge,
};
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
    ) -> Box<dyn Iterator<Item = &'a Edge<Node<MockEntryId>>> + Send + 'a>
    where
        MockEntryId: 'a,
    {
        Box::new(self.edge_index.locate_in_envelope_intersecting(&aabb))
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

impl Network<MockEntryId, MockMetadata> for MockNetwork {
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
                let direction_aware = DirectionAwareEdgeId::new(Node::new(
                    dummy_point(),
                    e.edge_id,
                ));
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
    use crate::r#match::MatchSimpleExt;
    use geo::{LineString, point, wkt};

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

    // ── Map-matching ──────────────────────────────────────────────────────────

    /// A GPS trajectory drifted slightly north of a straight road should snap
    /// back onto the road and produce a non-empty discretized matched path.
    ///
    /// The "discretized" path contains one matched element per input GPS point,
    /// even when consecutive candidates share the same edge.
    #[test]
    fn map_match_straight_road() {
        let net = straight_road();

        // Trajectory runs west along the road, offset ~33 m north
        // (0.0003° latitude ≈ 33 m, within the 50 m default search radius).
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.151 34.1503,
                -118.155 34.1503,
                -118.160 34.1503,
                -118.165 34.1503
            )
        };

        let result = net
            .match_simple(linestring)
            .expect("map match must succeed on a reachable network");

        // Every matched element should have metadata available.
        for element in &result.discretized.elements {
            assert!(
                net.metadata(element.edge.id()).is_some(),
                "metadata must be present for every matched edge"
            );
        }

        // One discretized element per GPS point — the match found a candidate
        // for each input position.
        assert_eq!(
            result.discretized.elements.len(),
            4,
            "discretized path must have one element per GPS input point"
        );
    }

    /// Two GPS points that project onto non-adjacent edges force the routing
    /// engine to traverse the intermediate edge.  The resulting interpolated
    /// path must include the edge that bridges the two candidates.
    #[test]
    fn map_match_interpolated_path_crosses_intermediate_edge() {
        // Four-node road:  1 ─(e1)─ 2 ─(e2)─ 3 ─(e3)─ 4
        let net = MockNetworkBuilder::new()
            .node(1, point!(x: -118.14, y: 34.15))
            .node(2, point!(x: -118.15, y: 34.15))
            .node(3, point!(x: -118.16, y: 34.15))
            .node(4, point!(x: -118.17, y: 34.15))
            .edge(1, 2)
            .edge(2, 3)
            .edge(3, 4)
            .build();

        // First GPS point sits near edge 1→2; second sits near edge 3→4.
        // Routing between them must traverse the intermediate edge 2→3.
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.141 34.1503,
                -118.169 34.1503
            )
        };

        let result = net
            .match_simple(linestring)
            .expect("map match must succeed");

        // The interpolated path must include at least one element from the
        // intermediate edge that connects the two candidate edges.
        assert!(
            !result.interpolated.elements.is_empty(),
            "interpolated path must be non-empty when candidates span non-adjacent edges"
        );

        // Every element in the interpolated path must have valid metadata.
        for element in &result.interpolated.elements {
            assert!(
                net.metadata(element.edge.id()).is_some(),
                "every interpolated edge must have metadata"
            );
        }
    }

    /// A T-junction network — the matcher should prefer the straight road over
    /// the branching turn when the GPS track continues straight.
    #[test]
    fn map_match_prefers_straight_over_turn() {
        //
        //  1 ── 2 ── 3      (straight road along lat=34.15)
        //       |
        //       4            (branch heading south)
        //
        let net = MockNetworkBuilder::new()
            .node(1, point!(x: -118.10, y: 34.15))
            .node(2, point!(x: -118.13, y: 34.15))
            .node(3, point!(x: -118.16, y: 34.15))
            .node(4, point!(x: -118.13, y: 34.12))
            .bidirectional_edge(1, 2)
            .bidirectional_edge(2, 3)
            .bidirectional_edge(2, 4)
            .build();

        // GPS track continues straight west, ~33 m north of the road
        // (0.0003° latitude ≈ 33 m, within the 50 m default search radius).
        // All points lie within the longitude range of edges 1→2 and 2→3
        // so that each point projects onto one of those edges.
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.101 34.1503,
                -118.111 34.1503,
                -118.121 34.1503,
                -118.131 34.1503,
                -118.141 34.1503,
                -118.151 34.1503,
                -118.158 34.1503
            )
        };

        let result = net
            .match_simple(linestring)
            .expect("map match must succeed");

        // A real match must have been made.
        assert!(
            !result.discretized.elements.is_empty(),
            "discretized path must be non-empty: a real match was made"
        );

        // The matched path must not contain node 4 (the south branch).
        let matched_node_ids: Vec<i64> = result
            .discretized
            .elements
            .iter()
            .flat_map(|e| [e.edge.source.id.0, e.edge.target.id.0])
            .collect();

        assert!(
            !matched_node_ids.contains(&4),
            "the south-branch node (4) must not appear in a straight-west trajectory match"
        );
    }

    /// A highway with a long offramp detour that exits and rejoins — the
    /// matcher should stay on the direct highway rather than routing through
    /// the detour, even though the highway curves slightly in the same
    /// direction (south) as the offramp exit.
    ///
    /// ```text
    ///  1 ──── 2 ──── 3 ──── 4     highway (bidirectional, curves slightly south)
    ///          \            ^
    ///           5 ──────── /      offramp (one-way south detour)
    /// ```
    ///
    /// GPS point 1 lands on edge 1–2; GPS point 2 lands on edge 3–4.  These
    /// two points span non-adjacent edges, so the routing layer must pick a
    /// path through the intermediate edges.  The highway middle (2→3, ~383 m)
    /// is far shorter than the offramp loop (2→5→4→3, ~1458 m), so the
    /// highway should win.
    ///
    /// # Node-spacing constraint
    ///
    /// The predicate cache is bounded to 2 km (200 000 cm).  Each edge here
    /// is kept well under that limit so the Dijkstra can reach every node.
    #[test]
    fn map_match_highway_preferred_over_offramp() {
        let net = MockNetworkBuilder::new()
            .node(1, point!(x: -118.100, y: 34.150))
            .node(2, point!(x: -118.105, y: 34.150)) // offramp exit (~459 m from 1)
            .node(3, point!(x: -118.109, y: 34.149)) // highway mid, curves south (~383 m from 2)
            .node(4, point!(x: -118.113, y: 34.148)) // highway end / offramp rejoin (~383 m from 3)
            .node(5, point!(x: -118.107, y: 34.146)) // offramp south detour (~481 m from 2)
            .bidirectional_edge(1, 2)
            .bidirectional_edge(2, 3)
            .bidirectional_edge(3, 4)
            .edge(2, 5) // one-way offramp exit
            .edge(5, 4) // one-way offramp rejoin
            .build();

        // GPS point 1: ~33 m north of edge 1–2 (offset 0.0003° lat).
        // GPS point 2: ~33 m north of edge 3–4 at lon = -118.111
        //   (edge y at that lon ≈ 34.1485; GPS at 34.1488).
        // The gap between the two candidates spans non-adjacent edges,
        // forcing the routing layer to choose between the highway and the detour.
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.102 34.1503,
                -118.111 34.1488
            )
        };

        let result = net
            .match_simple(linestring)
            .expect("map match must succeed");

        // Routing was forced across non-adjacent edges → the interpolated path
        // must contain at least the intermediate highway edge.
        assert!(
            !result.interpolated.elements.is_empty(),
            "interpolated path must be non-empty: routing spanned non-adjacent edges"
        );

        // The offramp detour (node 5) must not appear — the direct highway is preferred.
        let interpolated_node_ids: Vec<i64> = result
            .interpolated
            .elements
            .iter()
            .flat_map(|e| [e.edge.source.id.0, e.edge.target.id.0])
            .collect();

        assert!(
            !interpolated_node_ids.contains(&5),
            "offramp detour node (5) must not appear: the shorter highway route is preferred"
        );
    }

    /// A T-junction where the GPS track turns north — the matcher must follow
    /// the northward branch, not continue east.
    ///
    /// ```text
    ///               3   (north branch)
    ///               ^
    ///  1 ──── 2 ────┤
    ///               └── 4   (east continuation, wrong branch)
    /// ```
    ///
    /// The GPS approaches from the west along the horizontal road, then
    /// pivots northward.  A point placed 0.0003° east of the junction and
    /// 0.0003° north of the east road is a candidate for **both** the north
    /// branch (2→3, ~28 m away) and the east branch (2→4, ~43 m away via
    /// endpoint node 2).  The HMM must use the overall trajectory to resolve
    /// the ambiguity: assigning that point to the east branch would require
    /// a costly backtrack/U-turn to reach the subsequent north-branch points,
    /// so the north branch should be chosen.
    #[test]
    fn map_match_follows_turn_at_junction() {
        let net = MockNetworkBuilder::new()
            .node(1, point!(x: -118.10, y: 34.15)) // start (west)
            .node(2, point!(x: -118.13, y: 34.15)) // junction
            .node(3, point!(x: -118.13, y: 34.18)) // north branch (turn)
            .node(4, point!(x: -118.16, y: 34.15)) // east continuation (wrong branch)
            .bidirectional_edge(1, 2)
            .bidirectional_edge(2, 3)
            .bidirectional_edge(2, 4)
            .build();

        // Points 1–3: clearly on edge 1→2 (~33 m north of the road).
        // Point 4:    0.0003° east and 0.0003° north of node 2 — within 50 m
        //             of both edge 2→3 (north, ~28 m) and edge 2→4 (east,
        //             ~43 m via endpoint).  This is the ambiguous junction point.
        // Points 5–6: clearly on edge 2→3 (~28 m west of the north road),
        //             outside the 50 m radius of edge 2→4.
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.101 34.1503,
                -118.111 34.1503,
                -118.121 34.1503,
                -118.1297 34.1503,
                -118.1297 34.153,
                -118.1297 34.163
            )
        };

        let result = net
            .match_simple(linestring)
            .expect("map match must succeed");

        // A real match must be produced.
        assert!(
            !result.discretized.elements.is_empty(),
            "discretized path must be non-empty"
        );

        // The east-continuation node (4) must not appear in the matched path.
        // If the ambiguous junction point were incorrectly assigned to the east
        // branch (edge 2→4), node 4 would appear here and the subsequent
        // transition to the north branch would require a costly U-turn.
        let matched_node_ids: Vec<i64> = result
            .discretized
            .elements
            .iter()
            .flat_map(|e| [e.edge.source.id.0, e.edge.target.id.0])
            .collect();

        assert!(
            !matched_node_ids.contains(&4),
            "east-continuation node (4) must not appear when the GPS turns north at the junction"
        );
    }

    /// The interpolated path must include the edges that the GPS points are
    /// matched to (candidate edges), not just the routing edges between them.
    ///
    /// Before the fix, `interpolated` only contained the *bridging* edges that
    /// the router generated between consecutive candidate pairs.  The edges
    /// actually bearing the matched GPS positions were absent.
    ///
    /// ```text
    ///  1 ─(e1)─ 2 ─(e2)─ 3 ─(e3)─ 4
    /// ```
    ///
    /// GPS point 1 is near edge e1 (1→2); GPS point 2 is near edge e3 (3→4).
    /// The routing bridge is edge e2 (2→3).  The correct interpolated path is
    /// [e1, e2, e3] — all three edges in order.  Previously only [e2] was
    /// returned.
    #[test]
    fn map_match_interpolated_path_includes_candidate_edges() {
        // Four-node road:  1 ─(e1)─ 2 ─(e2)─ 3 ─(e3)─ 4
        let net = MockNetworkBuilder::new()
            .node(1, point!(x: -118.14, y: 34.15))
            .node(2, point!(x: -118.15, y: 34.15))
            .node(3, point!(x: -118.16, y: 34.15))
            .node(4, point!(x: -118.17, y: 34.15))
            .edge(1, 2)
            .edge(2, 3)
            .edge(3, 4)
            .build();

        // GPS point 1 sits near edge 1→2; GPS point 2 sits near edge 3→4.
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.141 34.1503,
                -118.169 34.1503
            )
        };

        let result = net
            .match_simple(linestring)
            .expect("map match must succeed");

        // The interpolated path must cover every edge traversed, including
        // the first and last candidate edges (e1 and e3).
        let interpolated_node_ids: Vec<i64> = result
            .interpolated
            .elements
            .iter()
            .flat_map(|e| [e.edge.source.id.0, e.edge.target.id.0])
            .collect();

        // e1 (1→2) must appear — it's the edge bearing GPS point 1.
        assert!(
            interpolated_node_ids.contains(&1),
            "node 1 (start of first candidate edge e1) must appear in the interpolated path"
        );

        // e3 (3→4) must appear — it's the edge bearing GPS point 2.
        assert!(
            interpolated_node_ids.contains(&4),
            "node 4 (end of last candidate edge e3) must appear in the interpolated path"
        );

        // e2 (2→3) must appear — it bridges the two candidate edges.
        assert!(
            interpolated_node_ids.contains(&2) && interpolated_node_ids.contains(&3),
            "nodes 2 and 3 (intermediate bridging edge e2) must appear in the interpolated path"
        );

        // Every element must have valid metadata.
        for element in &result.interpolated.elements {
            assert!(
                net.metadata(element.edge.id()).is_some(),
                "every interpolated edge must have metadata"
            );
        }
    }

    /// GPS trace that runs along a motorway (weight = 1) while a parallel
    /// motorway_link (weight = 2) road lies physically closer to the GPS points.
    ///
    /// Without road-class-weighted emission costs the motorway_link candidates
    /// would win purely on physical proximity.  With the fix, the emission cost
    /// of a candidate scales by the square of its road-class weight, so the
    /// motorway (weight 1² × physical distance) beats the motorway_link
    /// (weight 2² × smaller physical distance).
    ///
    /// ```text
    ///  1 ──── 2 ──── 3 ──── 4    motorway       (weight=1, ~7 m above GPS)
    ///  ·  GPS ·  GPS ·  GPS ·    GPS trace       (y = 34.15000)
    ///  5 ──── 6 ──── 7 ──── 8    motorway_link   (weight=2, ~4 m below GPS)
    /// ```
    ///
    /// The GPS points are ≈ 4.4 m from the motorway_link but ≈ 6.7 m from
    /// the motorway, so without weighting the motorway_link wins.  After
    /// quadratic weighting: effective distance for motorway_link =
    /// 4.4 m × 2² = 17.6 m vs motorway = 6.7 m × 1² = 6.7 m → motorway wins.
    #[test]
    fn map_match_prefers_motorway_over_motorway_link() {
        // Motorway nodes at y = 34.15006 (≈ 6.7 m north of GPS line)
        // MotorwayLink nodes at y = 34.14996 (≈ 4.4 m south of GPS line)
        let net = MockNetworkBuilder::new()
            .node(1, point!(x: -118.140, y: 34.15006))
            .node(2, point!(x: -118.141, y: 34.15006))
            .node(3, point!(x: -118.142, y: 34.15006))
            .node(4, point!(x: -118.143, y: 34.15006))
            // MotorwayLink (weight=2): parallel road slightly south
            .node(5, point!(x: -118.140, y: 34.14996))
            .node(6, point!(x: -118.141, y: 34.14996))
            .node(7, point!(x: -118.142, y: 34.14996))
            .node(8, point!(x: -118.143, y: 34.14996))
            .weighted_edge(1, 2, 1)
            .weighted_edge(2, 3, 1)
            .weighted_edge(3, 4, 1)
            .weighted_edge(5, 6, 2)
            .weighted_edge(6, 7, 2)
            .weighted_edge(7, 8, 2)
            .build();

        // GPS trace runs east, at y = 34.150000 — ≈ 4.4 m above motorway_link
        // (weight=2) but ≈ 6.7 m below the motorway (weight=1).  Without road-
        // class weighting the motorway_link wins on proximity alone.
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.1405 34.150000,
                -118.1415 34.150000,
                -118.1425 34.150000
            )
        };

        let result = net
            .match_simple(linestring)
            .expect("map match must succeed");

        // All matched edges must be the motorway (weight=1).
        // A motorway_link (weight=2) match means the road-class penalty
        // on emission costs is not being applied.
        for element in &result.discretized.elements {
            assert_eq!(
                element.edge.weight, 1,
                "matched edge must be the motorway (weight=1), not the motorway_link (weight=2)"
            );
        }
    }
}

