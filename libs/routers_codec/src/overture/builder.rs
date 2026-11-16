//! Assembles connectors + segments into an [`OvertureNetwork`].
//!
//! Each connector becomes a graph node. Each navigable segment is split at
//! its sorted connector positions, and the linestring's interior vertices
//! between two connectors are materialised as synthetic nodes, so the edge
//! chain follows the road shape rather than cutting the corner. Pure (no
//! IO), so it is unit-testable with hand-built elements.

use geo::Point;
use routers_network::columnar::{Graph, RawEdge};
use routers_network::{DirectionAwareEdgeId, Metadata, Node};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::overture::element::{Connector, Segment};
use crate::overture::graph::OvertureNetwork;
use crate::overture::id::OvertureEntryId;
use crate::overture::meta::OvertureEdgeMetadata;
use crate::overture::parsers::Heading;

/// Builds an [`OvertureNetwork`] from parsed elements.
///
/// Edge pairs referencing a connector absent from the connector set
/// (dangling references after a bbox clip) are dropped: every node in the
/// graph must carry a position, an invariant routing consumers rely on.
pub fn build(connectors: Vec<Connector>, segments: Vec<Segment>) -> OvertureNetwork {
    let mut hash: FxHashMap<OvertureEntryId, Node<OvertureEntryId>> = FxHashMap::default();
    for connector in &connectors {
        hash.insert(connector.id, Node::new(connector.position, connector.id));
    }

    // Synthetic (shape-vertex) node ids are allocated past every interned id
    // so they can never collide with a connector or segment.
    let mut next_synthetic = connectors
        .iter()
        .map(|c| c.id.identifier)
        .chain(segments.iter().map(|s| s.id.identifier))
        .max()
        .unwrap_or(-1)
        + 1;

    let mut edges = Vec::new();
    let mut ways: Vec<(OvertureEntryId, OvertureEdgeMetadata)> = Vec::new();

    for segment in &segments {
        if !segment.navigable() {
            continue;
        }

        let weight = segment.weight();
        let forward = segment.open(Heading::Forward);
        let backward = segment.open(Heading::Backward);
        let direction_aware = DirectionAwareEdgeId::new(segment.id);

        ways.push((segment.id, OvertureEdgeMetadata::pick(segment)));

        for pair in segment.connectors.windows(2) {
            if let [a, b] = pair {
                // Both endpoints must resolve to a positioned connector.
                if !hash.contains_key(&a.id) || !hash.contains_key(&b.id) {
                    continue;
                }

                // The chain a → shape vertices → b.
                let chain: Vec<OvertureEntryId> = core::iter::once(a.id)
                    .chain(segment.interior_vertices(a.at, b.at).map(|coord| {
                        let id = OvertureEntryId::new(next_synthetic);
                        next_synthetic += 1;
                        hash.insert(id, Node::new(Point(coord), id));
                        id
                    }))
                    .chain(core::iter::once(b.id))
                    .collect();

                for hop in chain.windows(2) {
                    if let [s, t] = hop {
                        let edge = |source, target, way| RawEdge {
                            source,
                            target,
                            class: weight,
                            way,
                        };

                        if forward {
                            edges.push(edge(*s, *t, direction_aware.forward()));
                        }
                        if backward {
                            edges.push(edge(*t, *s, direction_aware.backward()));
                        }
                    }
                }
            }
        }
    }

    // Keep only nodes that made it into the graph; isolated connectors would
    // otherwise bloat the node column and spatial index.
    let touched = edges
        .iter()
        .flat_map(|edge| [edge.source, edge.target])
        .collect::<FxHashSet<_>>();
    let nodes = hash
        .into_values()
        .filter(|node| touched.contains(&node.id))
        .collect();

    // A segment given twice keeps its last metadata.
    ways.reverse();

    OvertureNetwork {
        graph: Graph::assemble(nodes, edges, ways).indexed(),
    }
}
