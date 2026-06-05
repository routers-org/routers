use core::fmt::Debug;

use geo::Point;
use rstar::AABB;

use routers_network::{
    DataPlane, DirectionAwareEdgeId, Discovery, Edge, Entry, Metadata, Node, Route, Scan,
    edge::Weight, network::GraphEdge,
};

use super::MultiShardNetwork;
use crate::strategy::ShardId;

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
    type Meta = M;

    fn metadata(&self, id: &E) -> Option<&M> {
        // Walk shards on each call rather than copying every metadata
        // entry into the composite — the latter would roughly double
        // memory use. At typical window sizes (≤ 9 shards) this is a
        // handful of `HashMap::get` calls per lookup.
        self.shards.iter().find_map(|s| s.meta.get(id))
    }

    fn point(&self, id: &E) -> Option<Point> {
        self.hash.get(id).map(|n| n.position)
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
            source: *self.hash.get(&edge.source)?,
            target: *self.hash.get(&edge.target)?,
            id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), edge.id.index())),
            weight: edge.weight,
        })
    }
}

impl<E, M, S> Discovery<E> for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn edges_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Edge<Node<E>>> + Send + 'a>
    where
        E: 'a,
    {
        Box::new(self.index_edge.locate_in_envelope_intersecting(&aabb))
    }

    fn nodes_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Node<E>> + Send + 'a>
    where
        E: 'a,
    {
        Box::new(self.index.locate_in_envelope(&aabb))
    }

    fn node(&self, id: &E) -> Option<&Node<E>> {
        self.hash.get(id)
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

impl<E, M, S> Scan<E> for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<E>>
    where
        E: 'a,
    {
        self.index.nearest_neighbor(point)
    }
}

impl<E, M, S> Route<E> for MultiShardNetwork<E, M, S>
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
        let route = path
            .iter()
            .filter_map(|v| self.hash.get(v).copied())
            .collect();
        Some((cost, route))
    }
}
