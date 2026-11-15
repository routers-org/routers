//! [`CompactGraph`] as a [`Network`](routers_network::Network): a drop-in
//! for `OsmNetwork` wherever the matcher takes one.
//!
//! The network traits speak OSM ids and hand out `&Node` references, so this
//! wrapper adds what the bare columns lack: id-to-index maps, the nodes as
//! structs, and the two spatial indices. Entry, metadata and runtime types
//! are `OsmNetwork`'s own, so caches, options and results are
//! interchangeable.

use alloc::sync::Arc;
use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use geo::{Point, Rect};
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::edge::Weight;
use routers_network::network::GraphEdge;
use routers_network::{
    DataPlane, Direction, DirectionAwareEdgeId, Discovery, Edge, Metadata, Node, Route, RowIndex,
    Scan,
};
use rustc_hash::FxHashMap;
use uom::si::f64::Length;
use uom::si::length::meter;

use crate::{CompactGraph, EdgeIx, NO_NODE, NodeIx, Search};

type Runtime = <OsmEdgeMetadata as Metadata>::Runtime;

/// A [`CompactGraph`] with everything the network traits ask of it.
pub struct CompactNetwork {
    graph: CompactGraph,

    node_of: FxHashMap<OsmEntryId, NodeIx>,
    way_of: FxHashMap<OsmEntryId, u32>,
    way_ids: Vec<OsmEntryId>,
    /// The nodes as structs, for the trait methods that lend one out.
    nodes: Vec<Node<OsmEntryId>>,
    /// Per edge: the node it leaves.
    sources: Vec<NodeIx>,

    node_index: RowIndex<NodeIx>,
    edge_index: RowIndex<EdgeIx>,
    /// In-edges per node, built the first time they are asked for.
    incoming: OnceLock<Vec<Vec<EdgeIx>>>,

    /// Idle search workspaces, one per thread that has searched.
    searches: Mutex<Vec<Search>>,
    /// Which edges each runtime seen so far may traverse.
    passable: Mutex<Vec<(Runtime, Arc<[bool]>)>>,
}

impl core::fmt::Debug for CompactNetwork {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CompactNetwork")
            .field("nodes", &self.graph.nodes())
            .field("edges", &self.graph.edges())
            .finish_non_exhaustive()
    }
}

impl CompactNetwork {
    pub fn from_pbf(path: &Path) -> Result<Self, String> {
        CompactGraph::from_pbf(path).map(Self::new)
    }

    pub fn new(graph: CompactGraph) -> Self {
        let nodes = (0..graph.nodes() as NodeIx)
            .map(|node| Node::new(graph.point(node), OsmEntryId::node(graph.id(node))))
            .collect::<Vec<_>>();
        let node_of = nodes
            .iter()
            .enumerate()
            .map(|(node, entry)| (entry.id, node as NodeIx))
            .collect();

        let way_ids = graph
            .ways
            .iter()
            .map(|&id| OsmEntryId::way(id))
            .collect::<Vec<_>>();
        let way_of = way_ids
            .iter()
            .enumerate()
            .map(|(way, &id)| (id, way as u32))
            .collect();

        let mut sources = vec![0 as NodeIx; graph.edges()];
        for node in 0..graph.nodes() as NodeIx {
            for edge in graph.out_edges(node) {
                sources[edge as usize] = node;
            }
        }

        let (node_index, edge_index) = rayon::join(
            || {
                RowIndex::build((0..graph.nodes() as NodeIx).collect(), |&node| {
                    let at = graph.point(node);
                    (at, at)
                })
            },
            || graph.index_edges(),
        );

        CompactNetwork {
            graph,
            node_of,
            way_of,
            way_ids,
            nodes,
            sources,
            node_index,
            edge_index,
            incoming: OnceLock::new(),
            searches: Mutex::new(Vec::new()),
            passable: Mutex::new(Vec::new()),
        }
    }

    pub fn graph(&self) -> &CompactGraph {
        &self.graph
    }

    /// Run `search` with a workspace from the pool.
    fn searching<T>(&self, search: impl FnOnce(&mut Search) -> T) -> T {
        let idle = self
            .searches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let mut workspace = idle.unwrap_or_else(|| Search::new(&self.graph));

        let found = search(&mut workspace);

        self.searches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(workspace);
        found
    }

    /// The edges `runtime` may traverse, computed once per runtime.
    fn passable(&self, runtime: &Runtime) -> Arc<[bool]> {
        let known = |masks: &[(Runtime, Arc<[bool]>)]| {
            masks
                .iter()
                .find(|(seen, _)| seen == runtime)
                .map(|(_, mask)| Arc::clone(mask))
        };

        if let Some(mask) = known(&self.passable.lock().unwrap_or_else(PoisonError::into_inner)) {
            return mask;
        }

        // Built with the lock released. The mask is computed in parallel,
        // and a thread waiting on that work may be handed another match to
        // run meanwhile — one that would come straight back here for the
        // lock this thread still held.
        let mask: Arc<[bool]> = self.graph.accessibility(runtime).into();

        let mut masks = self.passable.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(raced) = known(&masks) {
            return raced;
        }
        masks.push((runtime.clone(), Arc::clone(&mask)));
        mask
    }

    fn way_id(&self, edge: EdgeIx) -> DirectionAwareEdgeId<OsmEntryId> {
        let way = self.graph.way[edge as usize] as usize;
        let id = self.way_ids.get(way).copied().unwrap_or_default();

        DirectionAwareEdgeId::new(id).with_direction(if self.graph.backward[edge as usize] {
            Direction::Incoming
        } else {
            Direction::Outgoing
        })
    }

    fn graph_edge(&self, edge: EdgeIx) -> GraphEdge<OsmEntryId> {
        (
            self.nodes[self.sources[edge as usize] as usize].id,
            self.nodes[self.graph.target(edge) as usize].id,
            (self.graph.class[edge as usize], self.way_id(edge)),
        )
    }
}

impl DataPlane for CompactNetwork {
    type Entry = OsmEntryId;
    type Runtime = Runtime;
    type Meta = OsmEdgeMetadata;

    fn metadata(&self, id: &OsmEntryId) -> Option<&OsmEdgeMetadata> {
        self.graph.meta.get(*self.way_of.get(id)? as usize)
    }

    fn point(&self, id: &OsmEntryId) -> Option<Point> {
        self.node_of.get(id).map(|&node| self.graph.point(node))
    }

    fn edges_outof<'a>(
        &'a self,
        id: OsmEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<OsmEntryId>> + 'a> {
        let edges = self
            .node_of
            .get(&id)
            .map(|&node| self.graph.out_edges(node));

        Box::new(
            edges
                .into_iter()
                .flatten()
                .map(|edge| self.graph_edge(edge)),
        )
    }

    fn edges_into<'a>(
        &'a self,
        id: OsmEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<OsmEntryId>> + 'a> {
        let incoming = self.incoming.get_or_init(|| {
            let mut incoming = vec![Vec::new(); self.graph.nodes()];
            for edge in 0..self.graph.edges() as EdgeIx {
                incoming[self.graph.target(edge) as usize].push(edge);
            }
            incoming
        });
        let edges = self.node_of.get(&id).map(|&node| &incoming[node as usize]);

        Box::new(
            edges
                .into_iter()
                .flatten()
                .map(|&edge| self.graph_edge(edge)),
        )
    }

    fn fatten(&self, edge: &Edge<OsmEntryId>) -> Option<Edge<Node<OsmEntryId>>> {
        Some(Edge {
            source: *self.node(&edge.source)?,
            target: *self.node(&edge.target)?,
            id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), edge.id.index())),
            weight: edge.weight,
        })
    }

    fn reach(
        &self,
        root: &OsmEntryId,
        reach: f64,
        runtime: &Runtime,
    ) -> Option<Vec<(OsmEntryId, OsmEntryId)>> {
        let root = *self.node_of.get(root)?;
        let passable = self.passable(runtime);

        Some(self.searching(|search| {
            search
                .reach(&self.graph, &passable, root, Length::new::<meter>(reach))
                .iter()
                .map(|&(node, parent)| {
                    let parent = match parent {
                        NO_NODE => OsmEntryId::default(),
                        parent => self.nodes[parent as usize].id,
                    };
                    (self.nodes[node as usize].id, parent)
                })
                .collect()
        }))
    }
}

impl Discovery for CompactNetwork {
    fn edges_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = Edge<Node<OsmEntryId>>> + Send + 'a> {
        Box::new(self.edge_index.search(bounds).map(|&edge| {
            let way = self.way_id(edge);

            Edge {
                source: self.nodes[self.sources[edge as usize] as usize],
                target: self.nodes[self.graph.target(edge) as usize],
                id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), way.index()))
                    .with_direction(way.direction()),
                weight: self.graph.class[edge as usize],
            }
        }))
    }

    fn nodes_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = &'a Node<OsmEntryId>> + Send + 'a> {
        Box::new(
            self.node_index
                .search(bounds)
                .map(|&node| &self.nodes[node as usize]),
        )
    }

    fn node(&self, id: &OsmEntryId) -> Option<&Node<OsmEntryId>> {
        self.node_of.get(id).map(|&node| &self.nodes[node as usize])
    }

    fn edge(&self, source: &OsmEntryId, target: &OsmEntryId) -> Option<Edge<OsmEntryId>> {
        let (from, to) = (*self.node_of.get(source)?, *self.node_of.get(target)?);
        let edge = self
            .graph
            .out_edges(from)
            .find(|&edge| self.graph.target(edge) == to)?;

        Some(Edge {
            source: *source,
            target: *target,
            weight: self.graph.class[edge as usize],
            id: self.way_id(edge),
        })
    }
}

impl Scan for CompactNetwork {
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<OsmEntryId>> {
        self.node_index
            .nearest(point)
            .map(|&node| &self.nodes[node as usize])
    }
}

impl Route for CompactNetwork {
    fn route_nodes(
        &self,
        start_node: OsmEntryId,
        finish_node: OsmEntryId,
    ) -> Option<(Weight, Vec<Node<OsmEntryId>>)> {
        let (from, to) = (
            *self.node_of.get(&start_node)?,
            *self.node_of.get(&finish_node)?,
        );
        let (cost, path) = self.searching(|search| search.route(&self.graph, from, to))?;

        Some((
            cost,
            path.into_iter()
                .map(|node| self.nodes[node as usize])
                .collect(),
        ))
    }
}
