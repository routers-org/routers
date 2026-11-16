//! The OSM routing network: a columnar [`Graph`] built from a `.osm.pbf`.

use core::fmt::Debug;

use log::debug;
use routers_network::columnar::{self, Graph};
use routers_network::network::GraphEdge;
use routers_network::{Metadata, Node, columnar_network};
use web_time::Instant;

#[cfg(not(target_arch = "wasm32"))]
use geo::Point;
#[cfg(not(target_arch = "wasm32"))]
use log::info;
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;
#[cfg(not(target_arch = "wasm32"))]
use routers_network::columnar::RawEdge;
#[cfg(not(target_arch = "wasm32"))]
use routers_network::{Direction, DirectionAwareEdgeId};
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

#[cfg(not(target_arch = "wasm32"))]
use crate::osm::element::ProcessedElement;
use crate::osm::*;

/// Magic header stapled at the start of every `.rt` file.
const SAVE_MAGIC: &[u8; 4] = b"OSMN";

// Prevent files from being used across build revisions
include!(concat!(env!("OUT_DIR"), "/format_hash.rs"));
const SAVE_VERSION: u64 = FORMAT_HASH ^ columnar::FORMAT;

#[derive(Default)]
pub struct OsmNetwork {
    graph: Graph<OsmEntryId, OsmEdgeMetadata>,
}

columnar_network!(impl[] OsmNetwork => graph: Graph<OsmEntryId, OsmEdgeMetadata>);

impl Debug for OsmNetwork {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("open street maps : network")
    }
}

impl OsmNetwork {
    /// Decode an encoded `OsmNetwork` from bytes. Its spatial indices are
    /// built on the first query that needs them.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        const HEADER_LEN: usize = SAVE_MAGIC.len() + 8;

        if bytes.len() < HEADER_LEN || &bytes[..SAVE_MAGIC.len()] != SAVE_MAGIC {
            return Err("Header bytes are missing, try rebuilding the cache.".to_string());
        }

        let version = u64::from_le_bytes(
            bytes[SAVE_MAGIC.len()..HEADER_LEN]
                .try_into()
                .expect("8 bytes"),
        );

        if version != SAVE_VERSION {
            return Err(format!(
                "Header expects {SAVE_VERSION:016x}, got format hash {version:016x}, rebuild the cache."
            ));
        }

        let started = Instant::now();
        let graph = Graph::decode(&mut &bytes[HEADER_LEN..])?;

        debug!(
            "OsmNetwork::from_bytes: {} bytes, deserialised in {:?}",
            bytes.len(),
            started.elapsed()
        );

        Ok(OsmNetwork { graph })
    }

    /// Encode `self` into a `Vec<u8>` with the format header prepended.
    /// Counterpart to [`from_bytes`](Self::from_bytes); filesystem-free.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();

        out.extend_from_slice(SAVE_MAGIC);
        out.extend_from_slice(&SAVE_VERSION.to_le_bytes());
        self.graph.encode(&mut out)?;

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
    pub fn from_pbf(filename: &PathBuf) -> Result<Self, Box<dyn core::error::Error>> {
        let started = Instant::now();

        let reader =
            ProcessedElementIterator::new(filename.clone()).map_err(|err| format!("{err:?}"))?;

        info!("Ingesting...");

        type Parts = (
            Vec<Node<OsmEntryId>>,
            Vec<RawEdge<OsmEntryId>>,
            Vec<(OsmEntryId, OsmEdgeMetadata)>,
        );

        let (nodes, edges, ways): Parts = reader.par_red(
            |mut parts: Parts, element: ProcessedElement| {
                match element {
                    ProcessedElement::Way(way) => {
                        let metadata = OsmEdgeMetadata::pick(way.tags());

                        let Some(class) = metadata.road_class.map(|class| class.weighting()) else {
                            return parts;
                        };

                        let bidirectional = !way.tags().unidirectional();
                        let along = |direction| {
                            DirectionAwareEdgeId::new(way.id()).with_direction(direction)
                        };
                        parts.2.push((way.id(), metadata));

                        for pair in way.refs().windows(2) {
                            let [a, b] = pair else { continue };

                            parts.1.push(RawEdge {
                                source: a.id,
                                target: b.id,
                                class,
                                way: along(Direction::Outgoing),
                            });
                            if bidirectional {
                                parts.1.push(RawEdge {
                                    source: b.id,
                                    target: a.id,
                                    class,
                                    way: along(Direction::Incoming),
                                });
                            }
                        }
                    }
                    ProcessedElement::Node(node) => parts.0.push(node),
                    _ => {}
                }

                parts
            },
            |mut a, b| {
                a.0.extend(b.0);
                a.1.extend(b.1);
                a.2.extend(b.2);
                a
            },
            || (Vec::new(), Vec::new(), Vec::new()),
        );

        debug!("Parsing took: {:?}", started.elapsed());

        let total = nodes.len();
        let nodes = endpoints(nodes, &edges);
        let network = OsmNetwork {
            graph: Graph::assemble(nodes, edges, ways),
        };

        info!(
            "Finished. Ingested {:?} nodes from {:?} nodes total in {}ms",
            network.num_nodes(),
            total,
            started.elapsed().as_millis()
        );

        Ok(network)
    }

    /// The columns the network is held as.
    pub fn graph(&self) -> &Graph<OsmEntryId, OsmEdgeMetadata> {
        &self.graph
    }

    pub fn num_nodes(&self) -> usize {
        self.graph.num_nodes()
    }

    pub fn num_edges(&self) -> usize {
        self.graph.num_edges()
    }

    /// Every node of the network.
    pub fn nodes(&self) -> impl Iterator<Item = &Node<OsmEntryId>> {
        self.graph.nodes()
    }

    /// Every directed edge of the network, grouped by the node it leaves.
    pub fn edges(&self) -> impl Iterator<Item = GraphEdge<OsmEntryId>> + '_ {
        self.graph.edges()
    }
}

/// The nodes that `edges` run between, each with its position among `nodes`.
/// The PBF also carries nodes no road touches.
#[cfg(not(target_arch = "wasm32"))]
fn endpoints(
    mut nodes: Vec<Node<OsmEntryId>>,
    edges: &[RawEdge<OsmEntryId>],
) -> Vec<Node<OsmEntryId>> {
    use core::sync::atomic::{AtomicBool, Ordering};

    nodes.par_sort_unstable_by_key(|node| node.id);
    nodes.dedup_by_key(|node| node.id);

    // Mark the nodes an edge touches where they stand, rather than gathering
    // every endpoint of every edge to sort: a flag a node, not two ids an
    // edge.
    let touched = (0..nodes.len())
        .map(|_| AtomicBool::new(false))
        .collect::<Vec<_>>();
    let mut absent = edges
        .par_iter()
        .flat_map_iter(|edge| [edge.source, edge.target])
        .filter(|id| match nodes.binary_search_by_key(id, |node| node.id) {
            Ok(found) => {
                touched[found].store(true, Ordering::Relaxed);
                false
            }
            Err(_) => true,
        })
        .collect::<Vec<_>>();
    absent.par_sort_unstable();
    absent.dedup();

    let mut touched = touched.into_iter().map(AtomicBool::into_inner);
    nodes.retain(|_| touched.next().unwrap_or(false));

    // An endpoint the file has no node for still takes its place, nowhere.
    nodes.extend(
        absent
            .into_iter()
            .map(|id| Node::new(Point::new(f64::NAN, f64::NAN), id)),
    );
    nodes.shrink_to_fit();
    nodes
}
