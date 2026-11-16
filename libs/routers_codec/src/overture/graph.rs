//! The Overture routing network.
//!
//! [`OvertureNetwork`] is a directed graph over interned connector ids,
//! carrying per-segment metadata, spatial node/edge indices, and a versioned
//! `.rt` on-disk format. It implements the `routers_network` traits
//! (`DataPlane`, `Discovery`, `Scan`, `Route`), so any consumer generic over
//! `N: Network` accepts it.

use core::fmt::Debug;

use log::debug;
use web_time::Instant;

use routers_network::columnar::{self, Graph};
use routers_network::network::GraphEdge;
use routers_network::{Node, columnar_network};

use crate::overture::element::{Connector, Segment};
use crate::overture::id::OvertureEntryId;
use crate::overture::meta::OvertureEdgeMetadata;

#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

#[cfg(all(feature = "overture", not(target_arch = "wasm32")))]
use crate::overture::error::OvertureError;

/// Magic header stapled at the start of every Overture `.rt` file.
const SAVE_MAGIC: &[u8; 4] = b"OVMN";

// Prevent files from being used across build revisions. Shares the crate-wide
// format hash emitted by `build.rs`.
include!(concat!(env!("OUT_DIR"), "/format_hash.rs"));
const SAVE_VERSION: u64 = FORMAT_HASH ^ columnar::FORMAT;

#[derive(Default)]
pub struct OvertureNetwork {
    pub(crate) graph: Graph<OvertureEntryId, OvertureEdgeMetadata>,
}

columnar_network!(impl[] OvertureNetwork => graph: Graph<OvertureEntryId, OvertureEdgeMetadata>);

impl OvertureNetwork {
    /// Build a network directly from parsed connectors and segments. Pure and
    /// filesystem-free — the unit-testable core of ingestion.
    pub fn from_elements(connectors: Vec<Connector>, segments: Vec<Segment>) -> Self {
        crate::overture::builder::build(connectors, segments)
    }

    /// Decode a previously-encoded `OvertureNetwork` from bytes and rebuild its
    /// spatial indices. Filesystem-free; suitable for WASM.
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

        let deserialise_start = Instant::now();
        let graph = Graph::decode(&mut &bytes[HEADER_LEN..])?;

        let deserialise = deserialise_start.elapsed();
        let rebuild_start = Instant::now();
        let net = OvertureNetwork {
            graph: graph.indexed(),
        };

        debug!(
            "OvertureNetwork::from_bytes: {} bytes, deserialised in {:?}, rebuilt indices in {:?}",
            bytes.len(),
            deserialise,
            rebuild_start.elapsed()
        );

        Ok(net)
    }

    /// Encode `self` into a `Vec<u8>` with the format header prepended.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();

        out.extend_from_slice(SAVE_MAGIC);
        out.extend_from_slice(&SAVE_VERSION.to_le_bytes());
        self.graph.encode(&mut out)?;

        Ok(out)
    }

    /// Read a directory (or single file) of Overture transportation GeoParquet
    /// and assemble a network. Not available on WASM; requires the `overture`
    /// feature (Arrow/Parquet reader stack).
    #[cfg(all(feature = "overture", not(target_arch = "wasm32")))]
    pub fn from_geoparquet(path: &Path) -> Result<Self, OvertureError> {
        let start = Instant::now();
        let transportation = crate::overture::reader::read_transportation(path)?;
        debug!(
            "OvertureNetwork::from_geoparquet read {} connectors, {} segments in {:?}",
            transportation.connectors.len(),
            transportation.segments.len(),
            start.elapsed()
        );
        Ok(Self::from_elements(
            transportation.connectors,
            transportation.segments,
        ))
    }

    /// Build from GeoParquet or, if a cache exists, load it; write the cache on
    /// a cold build.
    #[cfg(all(feature = "overture", not(target_arch = "wasm32")))]
    pub fn from_geoparquet_and_save(source: &Path, saved_path: &PathBuf) -> Result<Self, String> {
        if saved_path.exists() {
            match OvertureNetwork::from_saved(saved_path) {
                Ok(g) => return Ok(g),
                Err(e) => log::warn!(
                    "OvertureNetwork cache at `{}` is unusable ({e}); rebuilding from GeoParquet",
                    saved_path.display()
                ),
            }
        }
        let graph = OvertureNetwork::from_geoparquet(source).map_err(|e| e.to_string())?;
        graph.save_to_file(saved_path)?;
        Ok(graph)
    }

    /// Read a saved `.rt` from disk. Not available on WASM.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_saved(filename: &PathBuf) -> Result<Self, String> {
        let bytes = std::fs::read(filename).map_err(|v| v.to_string())?;
        Self::from_bytes(&bytes).map_err(|e| format!("cache file `{}`: {e}", filename.display()))
    }

    /// Persist this network to disk. Not available on WASM.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_to_file(&self, path: &Path) -> Result<(), String> {
        let bytes = self.to_bytes()?;
        let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        debug!(
            "OvertureNetwork::save_to_file wrote {} bytes (incl. 12-byte header, format {:016x}) to {}",
            bytes.len(),
            SAVE_VERSION,
            path.display()
        );
        Ok(())
    }

    pub fn num_nodes(&self) -> usize {
        self.graph.num_nodes()
    }

    pub fn num_edges(&self) -> usize {
        self.graph.num_edges()
    }

    /// Every node of the network.
    pub fn nodes(&self) -> impl Iterator<Item = &Node<OvertureEntryId>> {
        self.graph.nodes()
    }

    /// Every directed edge of the network, grouped by the node it leaves.
    pub fn edges(&self) -> impl Iterator<Item = GraphEdge<OvertureEntryId>> + '_ {
        self.graph.edges()
    }

    /// Every segment of the network with its metadata.
    pub fn segments(&self) -> impl Iterator<Item = (OvertureEntryId, &OvertureEdgeMetadata)> {
        self.graph.ways()
    }
}

impl Debug for OvertureNetwork {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("overture maps : network")
    }
}
