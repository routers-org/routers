//! The diff emission a matcher publishes per solve: one layer per trip
//! observation, addressable by timestamp, with hop edges and geometry
//! resolved against the matcher's network.

use geo::point;
use routers_network::DataPlane;
use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
use routers_realtime::event::MatchedDiff;
use routers_transition::costing::{CostingStrategies, DefaultEmissionCost, DefaultTransitionCost};
use routers_transition::layer::generation::StandardGenerator;
use routers_transition::weigh::AllCompute;
use routers_transition::{Matcher, Origin};

type Costing = CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, MockEntryId>;

/// A staircase road: west, then south, then west again.
fn bent_road() -> MockNetwork {
    MockNetworkBuilder::new()
        .node(1, point!(x: -118.15, y: 34.15))
        .node(2, point!(x: -118.16, y: 34.15))
        .node(3, point!(x: -118.17, y: 34.15))
        .node(4, point!(x: -118.17, y: 34.14))
        .node(5, point!(x: -118.18, y: 34.14))
        .edge(1, 2)
        .edge(2, 3)
        .edge(3, 4)
        .edge(4, 5)
        .build()
}

fn observations() -> Vec<Origin> {
    [
        point!(x: -118.151, y: 34.1503),
        point!(x: -118.155, y: 34.1503),
        point!(x: -118.165, y: 34.1503),
        point!(x: -118.170, y: 34.1490),
        point!(x: -118.172, y: 34.1403),
        point!(x: -118.179, y: 34.1403),
    ]
    .into_iter()
    .enumerate()
    // Realistic supplier stamps: distinct, spaced, non-zero micros.
    .map(|(index, point)| Origin::new(point, 1_775_000_000_000_000 + index as i64 * 5_000_000))
    .collect()
}

#[test]
fn diff_lifts_every_layer_with_its_timestamp() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    let origins = observations();
    let mut trip = m.begin();
    for &origin in &origins {
        m.push(&mut trip, origin).expect("push must anchor");
    }

    let solution = m.snapshot(&mut trip).expect("trip must solve");
    let diff = MatchedDiff::new(&solution, &origins, &net, 42);

    assert_eq!(diff.revision, 42);
    assert!(!diff.downgraded);
    assert_eq!(
        diff.layers.len(),
        origins.len(),
        "one emitted layer per observation"
    );

    for (layer, origin) in diff.layers.iter().zip(&origins) {
        assert_eq!(
            layer.timestamp, origin.timestamp,
            "layers are addressed by their observation's timestamp"
        );
    }

    assert!(
        diff.layers[0].path.is_empty(),
        "the first layer has no inbound hop"
    );

    // The staircase forces the interpolation through the bend: at least one
    // hop must resolve interior geometry, or the diff carries no more than
    // the raw positions did.
    assert!(
        diff.layers.iter().any(|layer| !layer.path.is_empty()),
        "hop geometry must resolve against the network"
    );
}

#[test]
fn diff_layers_flatten_into_a_connected_trace() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    let origins = observations();
    let mut trip = m.begin();
    for &origin in &origins {
        m.push(&mut trip, origin).expect("push must anchor");
    }

    let solution = m.snapshot(&mut trip).expect("trip must solve");
    let interpolated = solution.interpolated(&net);

    let diff = MatchedDiff::new(&solution, &origins, &net, 0);

    // Rendering the diff — each layer's path then its position, in timestamp
    // order — must reproduce the solution's own interpolated linestring.
    let rendered: Vec<geo::Point> = diff
        .layers
        .iter()
        .flat_map(|layer| {
            layer
                .path
                .iter()
                .copied()
                .chain(core::iter::once(layer.position))
        })
        .collect();

    let expected: Vec<geo::Point> = interpolated.into_points();
    assert_eq!(
        rendered, expected,
        "the diff must carry exactly the interpolated geometry"
    );
}

#[test]
fn hop_edges_name_the_edges_driven_between_layers() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    // The jump from the first edge to the third crosses edge 2 → 3 whole.
    let origins: Vec<Origin> = [
        point!(x: -118.151, y: 34.1503),
        point!(x: -118.155, y: 34.1503),
        point!(x: -118.1703, y: 34.145),
        point!(x: -118.179, y: 34.1403),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, point)| Origin::new(point, 1_775_000_000_000_000 + index as i64 * 5_000_000))
    .collect();

    let mut trip = m.begin();
    for &origin in &origins {
        m.push(&mut trip, origin).expect("push must anchor");
    }

    let solution = m.snapshot(&mut trip).expect("trip must solve");
    let diff = MatchedDiff::new(&solution, &origins, &net, 0);

    let id = |from: i64, to: i64| {
        net.edges_outof(MockEntryId(from))
            .find(|(_, target, _)| *target == MockEntryId(to))
            .map(|(_, _, (_, id))| id)
            .expect("edge is in the network")
    };

    let hops: Vec<_> = diff
        .layers
        .iter()
        .map(|layer| layer.hop_edges.clone())
        .collect();
    assert_eq!(
        hops,
        vec![vec![], vec![], vec![id(2, 3)], vec![]],
        "only the hop onto the third edge leaves its edge, through 2 → 3"
    );
}
