//! OSM adapter for the generic [`ShardSource`](crate::ShardSource).
//!
//! Bridges the codec's `ProcessedElementIterator` to the format-agnostic
//! node/way records expected by [`ShardedNetwork::from_source`]. The
//! filtering itself is intentionally kept on the *network* side: this
//! adapter just streams every node and every traversable way it finds.

use std::path::PathBuf;

use log::debug;
use routers_codec::osm::element::item::ProcessedElement;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, Parallel, ProcessedElementIterator};
use routers_network::Metadata;

use crate::source::{NodeRecord, ShardSource, WayRecord};

#[derive(Debug)]
pub struct OsmSource {
    path: PathBuf,
}

impl OsmSource {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[derive(Debug)]
pub enum OsmSourceError {
    Codec(String),
}

impl ShardSource for OsmSource {
    type Entry = OsmEntryId;
    type Metadata = OsmEdgeMetadata;
    type Error = OsmSourceError;

    fn for_each_node<F>(&self, mut f: F) -> Result<(), Self::Error>
    where
        F: FnMut(NodeRecord<Self::Entry>) + Send,
    {
        let reader = ProcessedElementIterator::new(self.path.clone())
            .map_err(|e| OsmSourceError::Codec(format!("{e:?}")))?;

        // Collect in parallel, then drain sequentially into the caller's
        // mutable closure. The codec already does this two-step inside
        // `OsmNetwork::from_pbf`; mirror it here to avoid forcing the
        // closure to be `Sync`.
        let nodes: Vec<NodeRecord<OsmEntryId>> = reader.par_red(
            |mut acc: Vec<NodeRecord<OsmEntryId>>, el: ProcessedElement| {
                if let ProcessedElement::Node(n) = el {
                    acc.push(NodeRecord {
                        id: n.id,
                        position: n.position,
                    });
                }
                acc
            },
            |mut a, b| {
                a.extend(b);
                a
            },
            Vec::new,
        );
        debug!("OsmSource yielded {} nodes", nodes.len());
        for n in nodes {
            f(n);
        }
        Ok(())
    }

    fn for_each_way<F>(&self, mut f: F) -> Result<(), Self::Error>
    where
        F: FnMut(WayRecord<Self::Entry, Self::Metadata>) + Send,
    {
        let reader = ProcessedElementIterator::new(self.path.clone())
            .map_err(|e| OsmSourceError::Codec(format!("{e:?}")))?;

        let ways: Vec<WayRecord<OsmEntryId, OsmEdgeMetadata>> = reader.par_red(
            |mut acc: Vec<WayRecord<OsmEntryId, OsmEdgeMetadata>>, el: ProcessedElement| {
                if let ProcessedElement::Way(w) = el {
                    let meta = OsmEdgeMetadata::pick(w.tags());
                    let Some(road_class) = meta.road_class else {
                        return acc;
                    };
                    let weight = road_class.weighting();
                    let bidirectional = !w.tags().unidirectional();
                    let refs = w.refs().iter().map(|r| r.id).collect::<Vec<_>>();
                    acc.push(WayRecord {
                        id: w.id(),
                        refs,
                        metadata: meta,
                        weight,
                        bidirectional,
                    });
                }
                acc
            },
            |mut a, b| {
                a.extend(b);
                a
            },
            Vec::new,
        );
        debug!("OsmSource yielded {} traversable ways", ways.len());
        for w in ways {
            f(w);
        }
        Ok(())
    }
}
