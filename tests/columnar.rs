//! A columnar network's own search must answer exactly as the matcher's does.

use std::sync::Arc;

use geo::{Point, Rect};
use routers::candidate::CandidateStore;
use routers::primitives::{PredicateCache, RoutingContext};
use routers::uom::si::f64::Length;
use routers::uom::si::length::meter;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{DISTRICT_OF_COLUMBIA, fixture};
use routers_network::edge::Weight;
use routers_network::network::GraphEdge;
use routers_network::{DataPlane, Discovery, Edge, Metadata, Network, Node, Route, Scan};
use routers_shard::{GeohashStrategy, MultiShardNetwork, ShardSource, ShardedNetwork, SourceEdge};

/// The same network with no search of its own, so the matcher runs its.
#[derive(Debug)]
struct Plain<'a, N>(&'a N);

impl<N: Network> DataPlane for Plain<'_, N> {
    type Entry = N::Entry;
    type Runtime = N::Runtime;
    type Meta = N::Meta;

    fn metadata(&self, id: &N::Entry) -> Option<&N::Meta> {
        self.0.metadata(id)
    }

    fn point(&self, id: &N::Entry) -> Option<Point> {
        self.0.point(id)
    }

    fn edges_outof<'a>(
        &'a self,
        id: N::Entry,
    ) -> Box<dyn Iterator<Item = GraphEdge<N::Entry>> + 'a> {
        self.0.edges_outof(id)
    }

    fn edges_into<'a>(
        &'a self,
        id: N::Entry,
    ) -> Box<dyn Iterator<Item = GraphEdge<N::Entry>> + 'a> {
        self.0.edges_into(id)
    }

    fn fatten(&self, edge: &Edge<N::Entry>) -> Option<Edge<Node<N::Entry>>> {
        self.0.fatten(edge)
    }
}

impl<N: Network> Discovery for Plain<'_, N> {
    fn edges_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = Edge<Node<N::Entry>>> + Send + 'a> {
        self.0.edges_in_box(bounds)
    }

    fn nodes_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = &'a Node<N::Entry>> + Send + 'a> {
        self.0.nodes_in_box(bounds)
    }

    fn node(&self, id: &N::Entry) -> Option<&Node<N::Entry>> {
        self.0.node(id)
    }

    fn edge(&self, source: &N::Entry, target: &N::Entry) -> Option<Edge<N::Entry>> {
        self.0.edge(source, target)
    }
}

impl<N: Network> Scan for Plain<'_, N> {
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<N::Entry>> {
        self.0.nearest_node(point)
    }
}

impl<N: Network> Route for Plain<'_, N> {
    fn route_nodes(
        &self,
        start: N::Entry,
        finish: N::Entry,
    ) -> Option<(Weight, Vec<Node<N::Entry>>)> {
        self.0.route_nodes(start, finish)
    }
}

/// `network` must reach from each of `roots` exactly what the matcher's own
/// search over it would.
fn searches_as_the_matcher_does<N: Network>(network: &N, runtime: &N::Runtime, roots: &[N::Entry]) {
    assert!(!roots.is_empty());
    assert!(
        network.reach(&roots[0], 500.0, runtime).is_some(),
        "the network has a search of its own"
    );

    let plain = Plain(network);
    let candidates = CandidateStore::default();

    for reach in [500.0, 1_000.0, 3_000.0] {
        let reach = Length::new::<meter>(reach);
        let own = PredicateCache::<N>::with_reach_distance(reach);
        let matchers = PredicateCache::<Plain<N>>::with_reach_distance(reach);

        for &root in roots {
            let own = own.query(
                &RoutingContext {
                    candidates: &candidates,
                    map: network,
                    runtime,
                },
                root,
            );
            let matchers = matchers.query(
                &RoutingContext {
                    candidates: &candidates,
                    map: &plain,
                    runtime,
                },
                root,
            );

            assert_eq!(own, matchers, "from {root:?}");
        }
    }
}

/// Some two hundred of `nodes`, spread evenly.
fn sample<E: Copy>(nodes: Vec<E>) -> Vec<E> {
    let step = nodes.len() / 200 + 1;
    nodes.into_iter().step_by(step).collect()
}

fn network() -> OsmNetwork {
    OsmNetwork::from_pbf(fixture!(DISTRICT_OF_COLUMBIA)).expect("pbf ingests")
}

struct OsmSource<'a>(&'a OsmNetwork);

impl ShardSource<OsmEntryId, OsmEdgeMetadata> for OsmSource<'_> {
    fn nodes<'b>(&'b self) -> Box<dyn Iterator<Item = (OsmEntryId, Point)> + 'b> {
        Box::new(self.0.nodes().map(|n| (n.id, n.position)))
    }

    fn edges<'b>(
        &'b self,
    ) -> Box<dyn Iterator<Item = SourceEdge<OsmEntryId, OsmEdgeMetadata>> + 'b> {
        Box::new(self.0.edges().filter_map(|(from, to, data)| {
            let meta = self.0.metadata(&data.1.index())?.clone();
            Some((from, to, data, meta))
        }))
    }
}

#[test]
fn an_osm_network_searches_as_the_matcher_does() {
    let network = network();
    let roots = sample(network.nodes().map(|node| node.id).collect());

    searches_as_the_matcher_does(&network, &OsmEdgeMetadata::default_runtime(), &roots);
}

#[test]
fn shards_search_as_the_matcher_does() {
    let network = network();
    let strategy = GeohashStrategy::with_precision(5);
    let runtime = OsmEdgeMetadata::default_runtime();

    let shards = ShardedNetwork::partition(&OsmSource(&network), &strategy, 250.0)
        .map(Arc::new)
        .collect::<Vec<_>>();
    assert!(shards.len() > 1, "the fixture spans shards");

    // One shard alone: routes stop at its padding.
    let largest = shards
        .iter()
        .max_by_key(|shard| shard.num_nodes())
        .expect("a shard");
    let roots = sample(largest.nodes().map(|node| node.id).collect());
    searches_as_the_matcher_does(&**largest, &runtime, &roots);

    // All of them as one: routes cross from shard to shard.
    let composite = MultiShardNetwork::new(shards);
    assert_eq!(composite.num_nodes(), network.num_nodes());
    assert_eq!(composite.num_edges(), network.num_edges());

    let roots = sample(network.nodes().map(|node| node.id).collect());
    searches_as_the_matcher_does(&composite, &runtime, &roots);

    // ...and reach what the unsharded network does.
    for &root in &roots {
        let mut whole = network.reach(&root, 1_000.0, &runtime).expect("searches");
        let mut joined = composite.reach(&root, 1_000.0, &runtime).expect("searches");
        whole.sort_unstable();
        joined.sort_unstable();

        assert_eq!(whole, joined, "from {root:?}");
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
