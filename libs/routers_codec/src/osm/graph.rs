use petgraph::prelude::DiGraphMap;
use routers_network::edge::Weight;
use routers_network::network::GraphEdge;
use routers_network::{DirectionAwareEdgeId, Discovery, Edge, Node, Route, Scan};

use log::debug;
use rstar::{AABB, RTree};
use rustc_hash::{FxHashMap, FxHasher};
use serde::{Deserialize, Serialize};

use core::fmt::Debug;
use core::hash::BuildHasherDefault;
use geo::Point;
use web_time::Instant;

#[cfg(not(target_arch = "wasm32"))]
use log::info;
#[cfg(not(target_arch = "wasm32"))]
use routers_network::Metadata;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

#[cfg(not(target_arch = "wasm32"))]
use crate::osm::element::ProcessedElement;
use crate::osm::*;

pub type GraphStructure<E> =
    DiGraphMap<E, (Weight, DirectionAwareEdgeId<E>), BuildHasherDefault<FxHasher>>;

/// Magic header stapled at the start of every `.rt` file.
const SAVE_MAGIC: &[u8; 4] = b"OSMN";

// Prevent files from being used across build revisions
include!(concat!(env!("OUT_DIR"), "/format_hash.rs"));
const SAVE_VERSION: u64 = FORMAT_HASH;

#[derive(Serialize, Deserialize)]
pub struct OsmNetwork {
    pub graph: GraphStructure<OsmEntryId>,
    pub hash: FxHashMap<OsmEntryId, Node<OsmEntryId>>,
    pub meta: FxHashMap<OsmEntryId, OsmEdgeMetadata>,

    /// Spatial index of nodes. Rebuilt on load — see [`Self::rebuild_indices`].
    /// Bulk-loading is faster than letting `postcard` decode the internal
    /// `rstar` tree, and it also shrinks the on-disk file substantially.
    #[serde(skip)]
    pub index: RTree<Node<OsmEntryId>>,
    #[serde(skip)]
    pub index_edge: RTree<Edge<Node<OsmEntryId>>>,
}

impl OsmNetwork {
    /// Decode a previously-encoded `OsmNetwork` from a byte slice and
    /// rebuild its spatial indices. Filesystem-free; suitable for WASM
    /// targets where bytes arrive via `fetch()` or similar.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        const HEADER_LEN: usize = SAVE_MAGIC.len() + 8;
        if bytes.len() < HEADER_LEN || &bytes[..SAVE_MAGIC.len()] != SAVE_MAGIC {
            return Err(
                "OsmNetwork bytes are missing the magic header — likely from a previous format. Rebuild the cache.".to_string()
            );
        }
        let version = u64::from_le_bytes(
            bytes[SAVE_MAGIC.len()..HEADER_LEN]
                .try_into()
                .expect("8 bytes"),
        );
        if version != SAVE_VERSION {
            return Err(format!(
                "OsmNetwork bytes have format hash {version:016x}, expected {SAVE_VERSION:016x}. The codec's serialised layout has changed — rebuild the cache."
            ));
        }
        let deserialise_start = Instant::now();
        let mut net: Self =
            postcard::from_bytes(&bytes[HEADER_LEN..]).map_err(|v| v.to_string())?;
        let deserialise = deserialise_start.elapsed();
        let rebuild_start = Instant::now();
        net.rebuild_indices();
        debug!(
            "OsmNetwork::from_bytes: {} bytes, deserialised in {:?}, rebuilt indices in {:?}",
            bytes.len(),
            deserialise,
            rebuild_start.elapsed()
        );
        Ok(net)
    }

    /// Encode `self` into a `Vec<u8>` with the format header prepended.
    /// Counterpart to [`from_bytes`](Self::from_bytes); filesystem-free.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let payload: Vec<u8> =
            postcard::to_allocvec(self).map_err(|e| format!("failed to serialise value: {e}"))?;
        let mut out = Vec::with_capacity(SAVE_MAGIC.len() + 8 + payload.len());
        out.extend_from_slice(SAVE_MAGIC);
        out.extend_from_slice(&SAVE_VERSION.to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Build an `OsmNetwork` either from a cached `.rt` file (fast path)
    /// or, if the cache is missing or stale, from the source PBF (slow
    /// path, writes the cache for next time).
    ///
    /// Filesystem-bound; not available on WASM targets — use
    /// [`from_bytes`](Self::from_bytes) with HTTP-fetched cache bytes
    /// instead.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_pbf_and_save(pbf_path: &PathBuf, saved_path: &PathBuf) -> Result<Self, String> {
        if saved_path.exists() {
            match OsmNetwork::from_saved(saved_path) {
                Ok(g) => return Ok(g),
                Err(e) => {
                    log::warn!(
                        "OsmNetwork cache at `{}` is unusable ({e}); rebuilding from PBF",
                        saved_path.display()
                    );
                }
            }
        }
        let graph = OsmNetwork::from_pbf(pbf_path).map_err(|e| e.to_string())?;
        graph.save_to_file(saved_path)?;
        Ok(graph)
    }

    /// Read a saved `.rt` from disk into an `OsmNetwork`. Thin wrapper
    /// around [`from_bytes`](Self::from_bytes); not available on WASM.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_saved(filename: &PathBuf) -> Result<Self, String> {
        let bytes = std::fs::read(filename).map_err(|v| v.to_string())?;
        Self::from_bytes(&bytes).map_err(|e| format!("cache file `{}`: {e}", filename.display()))
    }

    /// Rebuilds the node and edge spatial indices from `hash` and `graph`.
    /// Used after loading a serialised `OsmNetwork` (indices are skipped on
    /// the wire) and is safe to call at any time.
    pub fn rebuild_indices(&mut self) {
        // The two `RTree`s are independent — parallelise the bulk-load so
        // cache-hit cost is dominated by whichever tree is larger rather
        // than their sum.
        let nodes: Vec<Node<OsmEntryId>> = self.hash.values().copied().collect();
        let edges: Vec<Edge<Node<OsmEntryId>>> = self
            .graph
            .all_edges()
            .filter_map(|(s, t, &(weight, id))| {
                let source = *self.hash.get(&s)?;
                let target = *self.hash.get(&t)?;
                Some(Edge {
                    source,
                    target,
                    id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), id.index()))
                        .with_direction(id.direction()),
                    weight,
                })
            })
            .collect();
        let (node_index, edge_index) =
            rayon::join(|| RTree::bulk_load(nodes), || RTree::bulk_load(edges));
        self.index = node_index;
        self.index_edge = edge_index;
    }

    /// Persist this network to disk. Thin wrapper around
    /// [`to_bytes`](Self::to_bytes); not available on WASM.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_to_file(&self, path: &Path) -> Result<(), String> {
        let bytes = self.to_bytes()?;
        let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        debug!(
            "OsmNetwork::save_to_file wrote {} bytes (incl. 12-byte header, format {:016x}) to {}",
            bytes.len(),
            SAVE_VERSION,
            path.display()
        );
        Ok(())
    }

    /// Construct an `OsmNetwork` from a `.osm.pbf` file. Uses memory-mapped
    /// IO, multithreaded parsing and rayon; not available on WASM.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_pbf(filename: &PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let mut start_time = Instant::now();
        let fixed_start_time = Instant::now();

        let reader =
            ProcessedElementIterator::new(filename.clone()).map_err(|err| format!("{err:?}"))?;

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

    /// An empty network — zero nodes, zero edges, empty spatial indices.
    ///
    /// Cheap to construct and useful as a "still loading" placeholder when
    /// a viewer needs to be wired up before its real data has arrived
    /// (e.g. before an async shard fetch completes in the browser).
    pub fn empty() -> Self {
        Self {
            graph: GraphStructure::new(),
            hash: FxHashMap::default(),
            meta: FxHashMap::default(),
            index: RTree::new(),
            index_edge: RTree::new(),
        }
    }
}

impl Discovery<OsmEntryId> for OsmNetwork {
    fn edges_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Edge<Node<OsmEntryId>>> + Send + 'a>
    where
        OsmEntryId: 'a,
    {
        Box::new(self.index_edge.locate_in_envelope_intersecting(&aabb))
    }

    fn nodes_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Node<OsmEntryId>> + Send + 'a>
    where
        OsmEntryId: 'a,
    {
        Box::new(self.index.locate_in_envelope(&aabb))
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

impl Debug for OsmNetwork {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("open street maps : network")
    }
}

impl routers_network::DataPlane for OsmNetwork {
    type Entry = OsmEntryId;
    type Meta = OsmEdgeMetadata;

    fn metadata(&self, id: &OsmEntryId) -> Option<&OsmEdgeMetadata> {
        self.meta.get(id)
    }

    fn point(&self, id: &OsmEntryId) -> Option<Point> {
        self.hash.get(id).map(|v| v.position)
    }

    fn edges_into<'a>(
        &'a self,
        id: OsmEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<OsmEntryId>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Incoming)
                .map(|(src, dst, &data)| (src, dst, data)),
        )
    }

    fn edges_outof<'a>(
        &'a self,
        id: OsmEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<OsmEntryId>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Outgoing)
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
        }: &Edge<OsmEntryId>,
    ) -> Option<Edge<Node<OsmEntryId>>> {
        Some(Edge {
            source: *self.hash.get(source)?,
            target: *self.hash.get(target)?,
            id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), id.index())),
            weight: *weight,
        })
    }
}
