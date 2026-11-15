//! `OsmNetwork`'s own search must answer exactly as the matcher's does.

use geo::{Point, Rect};
use routers::candidate::CandidateStore;
use routers::primitives::{PredicateCache, RoutingContext};
use routers::uom::si::f64::Length;
use routers::uom::si::length::meter;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{DISTRICT_OF_COLUMBIA, fixture};
use routers_network::edge::Weight;
use routers_network::network::GraphEdge;
use routers_network::{DataPlane, Discovery, Edge, Metadata, Node, Route, Scan};

/// The same network with no search of its own, so the matcher runs its.
#[derive(Debug)]
struct Plain<'a>(&'a OsmNetwork);

impl DataPlane for Plain<'_> {
    type Entry = OsmEntryId;
    type Runtime = <OsmEdgeMetadata as Metadata>::Runtime;
    type Meta = OsmEdgeMetadata;

    fn metadata(&self, id: &OsmEntryId) -> Option<&OsmEdgeMetadata> {
        self.0.metadata(id)
    }

    fn point(&self, id: &OsmEntryId) -> Option<Point> {
        self.0.point(id)
    }

    fn edges_outof<'a>(
        &'a self,
        id: OsmEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<OsmEntryId>> + 'a> {
        self.0.edges_outof(id)
    }

    fn edges_into<'a>(
        &'a self,
        id: OsmEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<OsmEntryId>> + 'a> {
        self.0.edges_into(id)
    }

    fn fatten(&self, edge: &Edge<OsmEntryId>) -> Option<Edge<Node<OsmEntryId>>> {
        self.0.fatten(edge)
    }
}

impl Discovery for Plain<'_> {
    fn edges_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = Edge<Node<OsmEntryId>>> + Send + 'a> {
        self.0.edges_in_box(bounds)
    }

    fn nodes_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = &'a Node<OsmEntryId>> + Send + 'a> {
        self.0.nodes_in_box(bounds)
    }

    fn node(&self, id: &OsmEntryId) -> Option<&Node<OsmEntryId>> {
        self.0.node(id)
    }

    fn edge(&self, source: &OsmEntryId, target: &OsmEntryId) -> Option<Edge<OsmEntryId>> {
        self.0.edge(source, target)
    }
}

impl Scan for Plain<'_> {
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<OsmEntryId>> {
        self.0.nearest_node(point)
    }
}

impl Route for Plain<'_> {
    fn route_nodes(
        &self,
        start: OsmEntryId,
        finish: OsmEntryId,
    ) -> Option<(Weight, Vec<Node<OsmEntryId>>)> {
        self.0.route_nodes(start, finish)
    }
}

fn network() -> OsmNetwork {
    OsmNetwork::from_pbf(fixture!(DISTRICT_OF_COLUMBIA)).expect("pbf ingests")
}

#[test]
fn the_network_searches_as_the_matcher_does() {
    let network = network();
    let plain = Plain(&network);

    let runtime = OsmEdgeMetadata::default_runtime();
    let candidates = CandidateStore::default();
    let roots = network
        .nodes()
        .step_by(network.num_nodes() / 200 + 1)
        .map(|node| node.id)
        .collect::<Vec<_>>();

    for reach in [500.0, 1_000.0, 3_000.0] {
        let reach = Length::new::<meter>(reach);
        let own = PredicateCache::<OsmNetwork>::with_reach_distance(reach);
        let matchers = PredicateCache::<Plain>::with_reach_distance(reach);

        for &root in &roots {
            let own = own.query(
                &RoutingContext {
                    candidates: &candidates,
                    map: &network,
                    runtime: &runtime,
                },
                root,
            );
            let matchers = matchers.query(
                &RoutingContext {
                    candidates: &candidates,
                    map: &plain,
                    runtime: &runtime,
                },
                root,
            );

            assert_eq!(own, matchers, "from {root:?}");
        }
    }
}

#[test]
fn the_network_survives_the_disk() {
    let network = network();
    let bytes = network.to_bytes().expect("serialises");
    let restored = OsmNetwork::from_bytes(&bytes).expect("loads");

    assert_eq!(restored.num_nodes(), network.num_nodes());
    assert_eq!(restored.num_edges(), network.num_edges());
    assert!(restored.edges().eq(network.edges()));
    assert_eq!(restored.to_bytes().expect("serialises"), bytes);

    for cut in (0..bytes.len()).step_by(bytes.len() / 257 + 1) {
        assert!(
            OsmNetwork::from_bytes(&bytes[..cut]).is_err(),
            "cut at {cut}"
        );
    }

    let mut long = bytes.clone();
    long[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(OsmNetwork::from_bytes(&long).is_err());
}
