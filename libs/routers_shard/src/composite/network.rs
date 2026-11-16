use core::fmt::Debug;

use geo::{Point, Rect};
use rustc_hash::FxHashSet;

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
    type Runtime = M::Runtime;
    type Meta = M;

    fn metadata(&self, id: &E) -> Option<&M> {
        // Walk shards on each call rather than copying every metadata
        // entry into the composite — the latter would roughly double
        // memory use. At typical window sizes (≤ 9 shards) this is a
        // handful of lookups.
        self.shards.iter().find_map(|s| s.metadata(id))
    }

    fn point(&self, id: &E) -> Option<Point> {
        self.node(id).map(|n| n.position)
    }

    fn edges_outof<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(self.topology().edges_outof(id))
    }

    fn edges_into<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(self.topology().edges_into(id))
    }

    fn fatten(&self, edge: &Edge<E>) -> Option<Edge<Node<E>>> {
        self.topology().fatten(edge)
    }

    fn reach(&self, root: &E, reach: f64, runtime: &M::Runtime) -> Option<Vec<(E, E)>> {
        let passable = self.passable.get(runtime, || {
            let ways = self.topology().ways();

            self.topology().passable(|way, direction| {
                self.metadata(&ways[way])
                    .is_some_and(|meta| meta.accessible(runtime, direction))
            })
        });

        self.topology().reach(root, reach, &passable)
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
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = Edge<Node<E>>> + Send + 'a> {
        let mut seen: FxHashSet<(E, E)> = FxHashSet::default();

        Box::new(
            self.shards
                .iter()
                .flat_map(move |shard| shard.edges_in_box(bounds))
                .filter_map(move |found| {
                    if !seen.insert((found.source.id, found.target.id)) {
                        return None;
                    }

                    // Shards may each hold the edge: the composite's is the
                    // one a route would take.
                    let Edge { weight, id, .. } =
                        self.topology().edge(&found.source.id, &found.target.id)?;

                    let node = Node::new(Point::new(0., 0.), id.index());
                    let id = DirectionAwareEdgeId::new(node).with_direction(id.direction());

                    Some(Edge {
                        source: found.source,
                        target: found.target,
                        id,
                        weight,
                    })
                }),
        )
    }

    fn nodes_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = &'a Node<E>> + Send + 'a> {
        let mut seen: FxHashSet<E> = FxHashSet::default();

        Box::new(
            self.shards
                .iter()
                .flat_map(move |shard| shard.nodes_in_box(bounds))
                .filter(move |node| seen.insert(node.id)),
        )
    }

    fn node(&self, id: &E) -> Option<&Node<E>> {
        self.topology().node(id)
    }

    fn edge(&self, source: &E, target: &E) -> Option<Edge<E>> {
        self.topology().edge(source, target)
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
            .filter_map(|s| s.nearest_node(point))
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
        self.topology().route_nodes(start, finish)
    }
}
