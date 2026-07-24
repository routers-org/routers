use core::fmt::Debug;

use geo::Point;
use rstar::AABB;
use rustc_hash::FxHashSet;

use routers_network::{
    DataPlane, DirectionAwareEdgeId, Discovery, Edge, Entry, Metadata, Node, Route, Scan,
    edge::Weight, network::GraphEdge,
};

use super::MultiShardNetwork;
use crate::{network::EdgeRef, strategy::ShardId};

impl<E, M, S> Debug for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "MultiShardNetwork({} shards, {} nodes, {} edges)",
            self.shards.len(),
            self.num_nodes(),
            self.num_edges()
        )
    }
}

impl<E, M, S> DataPlane for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    type Entry = E;
    type Runtime = M::Runtime;
    type Meta = M;

    fn metadata(&self, id: &E) -> Option<&M> {
        // Walk shards on each call rather than copying every metadata
        // entry into the composite — the latter would roughly double
        // memory use. At typical window sizes (≤ 9 shards) this is a
        // handful of `HashMap::get` calls per lookup.
        self.shards.iter().find_map(|s| s.meta.get(id))
    }

    fn point(&self, id: &E) -> Option<Point> {
        self.node(id).map(|n| n.position)
    }

    fn edges_outof<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Outgoing)
                .map(|(s, t, &data)| (s, t, data)),
        )
    }

    fn edges_into<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Incoming)
                .map(|(s, t, &data)| (s, t, data)),
        )
    }

    fn fatten(&self, edge: &Edge<E>) -> Option<Edge<Node<E>>> {
        Some(Edge {
            source: *self.node(&edge.source)?,
            target: *self.node(&edge.target)?,
            id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), edge.id.index())),
            weight: edge.weight,
        })
    }
}

impl<E, M, S> Discovery for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn edges_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = Edge<Node<E>>> + Send + 'a> {
        let mut seen: FxHashSet<(E, E)> = FxHashSet::default();

        Box::new(
            self.shards
                .iter()
                .flat_map(move |shard| shard.index_edge.locate_in_envelope_intersecting(&aabb))
                .filter_map(move |&EdgeRef { source, target, .. }| {
                    if !seen.insert((source, target)) {
                        return None;
                    }

                    let source = *self.node(&source)?;
                    let target = *self.node(&target)?;

                    let &(weight, id) = self.graph.edge_weight(*source, *target)?;

                    let node = Node::new(Point::new(0., 0.), id.index());
                    let id = DirectionAwareEdgeId::new(node).with_direction(id.direction());

                    Some(Edge {
                        source,
                        target,
                        id,
                        weight,
                    })
                }),
        )
    }

    fn nodes_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Node<E>> + Send + 'a> {
        let mut seen: FxHashSet<E> = FxHashSet::default();

        Box::new(
            self.shards
                .iter()
                .flat_map(move |shard| shard.index.locate_in_envelope(&aabb))
                .filter(move |node| seen.insert(node.id)),
        )
    }

    fn node(&self, id: &E) -> Option<&Node<E>> {
        self.shards.iter().find_map(|s| s.hash.get(id))
    }

    fn edge(&self, source: &E, target: &E) -> Option<Edge<E>> {
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

impl<E, M, S> Scan for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<E>> {
        self.shards
            .iter()
            .filter_map(|s| s.index.nearest_neighbor(point))
            .min_by(|a, b| {
                let d2 = |n: &Node<E>| {
                    (n.position.x() - point.x()).powi(2) + (n.position.y() - point.y()).powi(2)
                };
                d2(a).total_cmp(&d2(b))
            })
    }
}

impl<E, M, S> Route for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn route_nodes(&self, start: E, finish: E) -> Option<(Weight, Vec<Node<E>>)> {
        let (cost, path) = petgraph::algo::astar(
            &self.graph,
            start,
            |n| n == finish,
            |(_, _, w)| w.0,
            |_| 0 as Weight,
        )?;
        let route = path.iter().filter_map(|v| self.node(v).copied()).collect();
        Some((cost, route))
    }
}
