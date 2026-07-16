//! Streaming (realtime) map-matching, for when positions arrive one at a time.

use geo::{Point, point};
use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
use routers_transition::generation::StandardGenerator;
use routers_transition::{
    AllCompute, CostingStrategies, DEFAULT_SEARCH_DISTANCE, MatchError, Matcher, Trip,
};

/// A staircase road: west along lat 34.15, south, then west again.
fn road() -> MockNetwork {
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

/// The position stream: a drifted trace with one GPS glitch in the middle.
fn stream() -> Vec<Point> {
    vec![
        point!(x: -118.151, y: 34.1503),
        point!(x: -118.155, y: 34.1503),
        point!(x: -118.165, y: 34.1503),
        point!(x: 0.0, y: 0.0), // example unanchored node that's nowhere near the network
        point!(x: -118.170, y: 34.1490),
        point!(x: -118.172, y: 34.1403),
        point!(x: -118.179, y: 34.1403),
    ]
}

fn main() -> Result<(), MatchError> {
    let network = road();
    let costing = CostingStrategies::default();
    let generator = StandardGenerator::new(&network, &costing.emission, DEFAULT_SEARCH_DISTANCE);
    let matcher = Matcher::new(&network, &costing, generator, AllCompute::default(), &());

    let mut trip = matcher.begin();

    for (tick, position) in stream().into_iter().enumerate() {
        // `push` is atomic, it either adds the position, or fails.
        let layer = match matcher.push(&mut trip, position) {
            Ok(layer) => layer,
            Err(MatchError::Unanchored(e)) => {
                println!("tick {tick}: dropped off-network point ({e})");
                continue;
            }
            Err(e) => return Err(e),
        };

        // We can then solve for the next point, which will update our trip.
        let (cost, points) = {
            let path = matcher.solve(&mut trip)?;
            (path.cost, path.nodes.len())
        };

        println!(
            "tick {tick}: layer {layer} ({} candidates): cost={cost} points={points}",
            trip.layer(layer).map_or(0, <[_]>::len),
        );

        // As an example, we can persist the trip at any time (arbitrary tick), and
        // resume it later, without needing to rebuild the matcher or caches.
        if tick == 2 {
            let stored = serde_json::to_string(&trip).expect("trip serializes");
            println!(
                "-> persisted trip at tick={tick} into {} bytes of JSON",
                stored.len()
            );
            trip = serde_json::from_str::<Trip<MockEntryId>>(&stored).expect("trip deserializes");
            assert!(trip.is_solved(), "solved state survives persistence");
        }
    }

    // We can also inspect the trip's path, i.e. to determine
    // the nodes travelled, or derive any hop within the current path.
    if let Some(path) = trip.path() {
        let route = path
            .nodes
            .iter()
            .enumerate()
            .map(|(l, &n)| {
                routers_transition::CandidateRef::new(routers_transition::LayerId(l as u32), n)
            })
            .collect::<Vec<_>>();

        if let [.., from, to] = route.as_slice() {
            let hop = matcher.hop(&trip, *from, *to);
            println!(
                "latest hop {from}→{to}: {} routed edge(s)",
                hop.map_or(0, |r| r.path.len()),
            );
        }
    }

    // We can also collapse the current solution into a full match result,
    // re-deriving all hop geometry from the warm predicate cache. The trip
    // stays usable afterwards, so this works mid-stream too.
    let collapsed = matcher.snapshot(&mut trip)?;
    println!(
        "snapshot: cost {}, {} matched points, {} hops",
        collapsed.cost,
        collapsed.route.len(),
        collapsed.interpolated.len(),
    );

    println!("interpolated: {:?}", collapsed.interpolated);
    println!("collapsed: {:?}", collapsed.collapsed());

    Ok(())
}
