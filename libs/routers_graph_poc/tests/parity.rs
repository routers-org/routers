//! The compact graph must be the same network as `OsmNetwork`, and search it
//! to the same answers.

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{DISTRICT_OF_COLUMBIA, fixture};
use routers_graph_poc::{CompactGraph, NO_NODE, Search};
use routers_network::{Entry, Metadata, Route};
use routers_transition::Matcher;
use routers_transition::costing::CostingStrategies;
use routers_transition::layer::generation::StandardGenerator;
use routers_transition::primitives::{PredicateCache, RoutingContext};
use routers_transition::uom::si::f64::Length;
use routers_transition::uom::si::length::meter;
use routers_transition::weigh::AllCompute;
use rustc_hash::FxHashMap;

#[test]
fn the_compact_graph_is_the_same_network() {
    let pbf = fixture!(DISTRICT_OF_COLUMBIA);
    let network = OsmNetwork::from_pbf(pbf).expect("pbf ingests");
    let compact = CompactGraph::from_pbf(pbf).expect("pbf ingests");

    // Ingested directly, or laid out from the existing network: one layout.
    assert_eq!(compact.nodes(), network.num_nodes());
    assert!(compact.same_layout(&CompactGraph::from_network(&network)));

    // And it survives the disk.
    let loaded = CompactGraph::from_bytes(&compact.to_bytes()).expect("loads");
    assert!(loaded.same_layout(&compact));

    let runtime = OsmEdgeMetadata::default_runtime();
    let passable = compact.accessibility(&runtime);
    let lookup = compact.lookup();

    let costing = CostingStrategies::default();
    let generator = StandardGenerator::new(&network, &costing.emission);
    let matcher = Matcher::new(
        &network,
        &costing,
        generator,
        AllCompute::default(),
        &runtime,
    );
    let trip = matcher.begin();
    let ctx = RoutingContext {
        candidates: trip.candidates(),
        map: &network,
        runtime: &runtime,
    };

    let roots = network
        .graph
        .nodes()
        .step_by(network.num_nodes() / 200)
        .collect::<Vec<OsmEntryId>>();
    let mut search = Search::new(&compact);

    // The matcher's bounded search: same nodes, same parents, at any reach.
    for reach in [500.0, 1_000.0, 3_000.0] {
        let reach = Length::new::<meter>(reach);
        let cache = PredicateCache::<OsmNetwork>::with_reach_distance(reach);

        for &root in &roots {
            let node = lookup.node(root).expect("root is a node");

            let compact_parents = search
                .reach(&compact, &passable, node, reach)
                .iter()
                .map(|&(node, parent)| {
                    let parent = if parent == NO_NODE {
                        OsmEntryId::default().identifier()
                    } else {
                        compact.id(parent)
                    };
                    (compact.id(node), parent)
                })
                .collect::<FxHashMap<_, _>>();
            let existing_parents = cache
                .query(&ctx, root)
                .iter()
                .map(|(node, parent)| (node.identifier(), parent.identifier()))
                .collect::<FxHashMap<_, _>>();

            assert_eq!(
                compact_parents,
                existing_parents,
                "from {}",
                root.identifier()
            );
        }
    }

    // Point-to-point routing: the same cost, over a path that exists.
    for pair in roots.windows(2).take(25) {
        let [from, to] = pair else { continue };
        let existing = network.route_nodes(*from, *to).map(|(cost, _)| cost);

        let route = search.route(
            &compact,
            lookup.node(*from).expect("node"),
            lookup.node(*to).expect("node"),
        );
        assert_eq!(route.as_ref().map(|(cost, _)| *cost), existing);

        if let Some((_, path)) = route {
            for hop in path.windows(2) {
                assert!(
                    compact
                        .out_edges(hop[0])
                        .any(|edge| compact.target(edge) == hop[1]),
                    "the route follows edges"
                );
            }
        }
    }
}

/// Bytes off a disk may be anything: a graph that does not hold together is
/// refused when it is read, not discovered when it is searched.
#[test]
fn corrupt_bytes_are_refused() {
    let compact = CompactGraph::from_pbf(fixture!(DISTRICT_OF_COLUMBIA)).expect("pbf ingests");
    let bytes = compact.to_bytes();

    for cut in (0..bytes.len()).step_by(bytes.len() / 257 + 1) {
        assert!(
            CompactGraph::from_bytes(&bytes[..cut]).is_err(),
            "cut at {cut}"
        );
    }

    // A column claiming more entries than could exist.
    let mut long = bytes.clone();
    long[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(CompactGraph::from_bytes(&long).is_err());

    // An edge leading nowhere.
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(CompactGraph::from_bytes(&trailing).is_err());

    assert!(CompactGraph::from_bytes(&bytes).is_ok());
}
