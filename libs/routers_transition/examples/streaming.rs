//! Streaming (realtime) map-matching: positions arrive one at a time.
//!
//! The caller owns a [`Trip`] — pure, serializable data — and hands it back to
//! a [`Matcher`] each tick: `push` appends the position's candidate layer,
//! `solve` weighs whatever is pending and re-certifies the minimum-cost path.
//! Solving is *defined* as weigh-then-solve, so there is no weighing step to
//! forget, and already-weighed boundaries are never recomputed — a tick costs
//! one boundary's weighing plus a µs-scale DP pass.
//!
//! Along the way this demonstrates the supporting machinery a realtime service
//! needs: rejecting off-network points without corrupting the trip, persisting
//! a trip mid-stream (serde) and resuming it elsewhere, and re-deriving hop
//! geometry per tick.
//!
//! Run: `cargo run -p routers_transition --example streaming`

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
        point!(x: 0.0, y: 0.0), // glitch: nowhere near the network
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
        // `push` is atomic: candidates, trellis layer, and emission node
        // weights land together, or — for an unanchorable point — not at all,
        // so the stream just drops the glitch and carries on.
        let layer = match matcher.push(&mut trip, position) {
            Ok(layer) => layer,
            Err(MatchError::Unanchored(e)) => {
                println!("tick {tick}: dropped off-network point ({e})");
                continue;
            }
            Err(e) => return Err(e),
        };

        // Weigh the new boundary and re-certify the best path. The HMM may
        // revise earlier choices as new evidence arrives — that is the point.
        let (cost, points) = {
            let path = matcher.solve(&mut trip)?;
            (path.cost, path.nodes.len())
        };
        println!(
            "tick {tick}: layer {layer} ({} candidates) → cost {cost} over {points} points",
            trip.layer(layer).map_or(0, <[_]>::len),
        );

        // Persist the trip at an arbitrary tick boundary — it is pure data —
        // and resume it as a "new process" would (fresh matcher and caches).
        if tick == 2 {
            let stored = serde_json::to_string(&trip).expect("trip serializes");
            println!("        persisted trip: {} bytes of JSON", stored.len());
            trip = serde_json::from_str::<Trip<MockEntryId>>(&stored).expect("trip deserializes");
            assert!(trip.is_solved(), "solved state survives persistence");
        }
    }

    // Per-tick interpolated output, if a consumer wants it, is the caller's to
    // derive (and memoise): re-derive any hop of the current path on demand.
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

    // Trip complete: collapse into the final match, re-deriving all hop
    // geometry from the warm predicate cache.
    let collapsed = matcher.finish(trip)?;
    println!(
        "finished: cost {}, {} matched points, {} hops",
        collapsed.cost,
        collapsed.route.len(),
        collapsed.interpolated.len(),
    );

    Ok(())
}
