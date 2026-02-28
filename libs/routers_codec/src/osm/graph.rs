use petgraph::prelude::DiGraphMap;
use routers_network::edge::Weight;
use routers_network::{
    DirectionAwareEdgeId, Discovery, Edge, Metadata, Network, Node, Route, Scan,
};

use log::{debug, info};
use rstar::{AABB, RTree};
use rustc_hash::{FxHashMap, FxHasher};

use core::error::Error;
use geo::Point;
use std::hash::BuildHasherDefault;
use std::path::PathBuf;
use std::time::Instant;

use crate::osm::element::ProcessedElement;
use crate::osm::*;

pub type GraphStructure<E> =
    DiGraphMap<E, (Weight, DirectionAwareEdgeId<E>), BuildHasherDefault<FxHasher>>;

pub struct OsmNetwork {
    pub graph: GraphStructure<OsmEntryId>,
    pub hash: FxHashMap<OsmEntryId, Node<OsmEntryId>>,
    pub meta: FxHashMap<OsmEntryId, OsmEdgeMetadata>,

    pub index: RTree<Node<OsmEntryId>>,
    pub index_edge: RTree<Edge<Node<OsmEntryId>>>,
}

impl OsmNetwork {
    /// Creates a graph from a `.osm.pbf` file, using the `ProcessedElementIterator`
    pub fn new(filename: std::ffi::OsString) -> Result<Self, Box<dyn Error>> {
        let mut start_time = Instant::now();
        let fixed_start_time = Instant::now();

        let path = PathBuf::from(filename);

        let reader = ProcessedElementIterator::new(path).map_err(|err| format!("{err:?}"))?;

        debug!("Iterator warming took: {:?}", start_time.elapsed());
        start_time = Instant::now();

        info!("Ingesting...");

        let (nodes, edges, metadata): (
            Vec<Node<OsmEntryId>>,
            Vec<Edge<OsmEntryId>>,
            Vec<(OsmEntryId, OsmEdgeMetadata)>,
        ) = reader.par_red(
            |mut trees: (
                Vec<Node<OsmEntryId>>,
                Vec<Edge<OsmEntryId>>,
                Vec<(OsmEntryId, OsmEdgeMetadata)>,
            ),
             element: ProcessedElement| {
                match element {
                    ProcessedElement::Way(way) => {
                        let metadata = OsmEdgeMetadata::pick(way.tags());
                        // If way is not traversable (/ is not road)
                        if metadata.road_class.is_none() {
                            return trees;
                        }

                        // Get the weight from the weight table
                        let weight = metadata.road_class.unwrap().weighting();

                        let bidirectional = !way.tags().unidirectional();
                        trees.2.push((way.id(), metadata));

                        // Update with all adjacent nodes
                        way.refs().windows(2).for_each(|edge| {
                            if let [a, b] = edge {
                                let direction_aware = DirectionAwareEdgeId::new(way.id());

                                let w = (weight, direction_aware.forward());
                                trees.1.push(Edge::from((a.id, b.id, &w)));

                                // If way is bidi, add opposite edge with a DirAw backward.
                                if bidirectional {
                                    let w = (weight, direction_aware.backward());
                                    trees.1.push(Edge::from((b.id, a.id, &w)));
                                }
                            } else {
                                debug!("Edge windowing produced odd-sized entry: {edge:?}");
                            }
                        });
                    }
                    ProcessedElement::Node(node) => {
                        // Add the node to the graph
                        trees.0.push(node);
                    }
                    _ => {}
                }

                trees
            },
            |mut a_tree, b_tree| {
                a_tree.0.extend(b_tree.0);
                a_tree.1.extend(b_tree.1);
                a_tree.2.extend(b_tree.2);
                a_tree
            },
            || (Vec::new(), Vec::new(), Vec::new()),
        );

        let mut graph = GraphStructure::new();
        for edge in &edges {
            graph.add_edge(edge.source, edge.target, (edge.weight, edge.id));
        }

        debug!("Graphical ingestion took: {:?}", start_time.elapsed());
        start_time = Instant::now();

        let meta = metadata.into_iter().collect::<FxHashMap<_, _>>();

        let mut hash = FxHashMap::default();
        let filtered = {
            nodes
                .iter()
                .copied()
                .filter(|v| graph.contains_node(v.id))
                .inspect(|e| {
                    hash.insert(e.id, *e);
                })
                .collect()
        };

        let fat = {
            edges
                .iter()
                .flat_map(|edge| {
                    Some(Edge {
                        source: *hash.get(&edge.source)?,
                        target: *hash.get(&edge.target)?,
                        id: DirectionAwareEdgeId::new(Node::new(
                            Point::new(0., 0.),
                            edge.id.index(),
                        ))
                        .with_direction(edge.id.direction()),
                        weight: edge.weight,
                    })
                })
                .collect()
        };

        debug!("HashMap creation took: {:?}", start_time.elapsed());
        start_time = Instant::now();

        let tree = RTree::bulk_load(filtered);
        let tree_edge = RTree::bulk_load(fat);
        debug!("RTree bulk load took: {:?}", start_time.elapsed());

        info!(
            "Finished. Ingested {:?} nodes from {:?} nodes total in {}ms",
            tree.size(),
            nodes.len(),
            fixed_start_time.elapsed().as_millis()
        );

        Ok(OsmNetwork {
            graph,
            hash,

            meta,

            index: tree,
            index_edge: tree_edge,
        })
    }

    pub fn num_nodes(&self) -> usize {
        self.graph.node_count()
    }
}

impl Discovery<OsmEntryId> for OsmNetwork {
    fn edges_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> impl Iterator<Item = &'a Edge<Node<OsmEntryId>>>
    where
        OsmEntryId: 'a,
    {
        self.index_edge.locate_in_envelope(&aabb)
    }

    fn nodes_in_box<'a>(&'a self, aabb: AABB<Point>) -> impl Iterator<Item = &'a Node<OsmEntryId>>
    where
        OsmEntryId: 'a,
    {
        self.index.locate_in_envelope(&aabb)
    }

    fn node(&self, id: &OsmEntryId) -> Option<&Node<OsmEntryId>> {
        self.hash.get(id)
    }

    fn edge(&self, &source: &OsmEntryId, &target: &OsmEntryId) -> Option<Edge<OsmEntryId>> {
        self.graph
            .edge_weight(source, target)
            .map(|&(weight, id)| Edge {
                source,
                target,
                weight,
                id,
            })
    }
}

impl Scan<OsmEntryId> for OsmNetwork {
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<OsmEntryId>>
    where
        OsmEntryId: 'a,
    {
        self.index.nearest_neighbor(point)
    }
}

impl Route<OsmEntryId> for OsmNetwork {
    fn route_nodes(
        &self,
        start_node: OsmEntryId,
        finish_node: OsmEntryId,
    ) -> Option<(Weight, Vec<Node<OsmEntryId>>)> {
        let (score, path) = petgraph::algo::astar(
            &self.graph,
            start_node,
            |finish| finish == finish_node,
            |(_, _, w)| w.0,
            |_| 0 as Weight,
        )?;

        let route = path
            .iter()
            .filter_map(|v| self.hash.get(v).copied())
            .collect();

        Some((score, route))
    }
}

impl Network<OsmEntryId, OsmEdgeMetadata> for OsmNetwork {
    fn metadata(&self, id: &OsmEntryId) -> Option<&OsmEdgeMetadata> {
        self.meta.get(id)
    }

    fn point(&self, id: &OsmEntryId) -> Option<Point> {
        self.hash.get(id).map(|v| v.position)
    }
}
