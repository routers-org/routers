//! Full map-matching. One trajectory, one matched route.

use geo::{LineString, point, wkt};
use routers_network::mock::{MockNetwork, MockNetworkBuilder};
use routers_transition::generation::StandardGenerator;
use routers_transition::r#match::MatchSimpleExt;
use routers_transition::{
    AllCompute, CostingStrategies, DEFAULT_SEARCH_DISTANCE, MatchError, Matcher,
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

/// A GPS trace drifted ~30m off the road.
fn trace() -> LineString {
    wkt! {
        LINESTRING(
            -118.151 34.1503, -118.155 34.1503, -118.165 34.1503,
            -118.170 34.1490, -118.172 34.1403, -118.179 34.1403
        )
    }
}

fn print_line(label: &str, line: &LineString) {
    let coords = line
        .coords()
        .map(|c| format!("({:.4}, {:.4})", c.x, c.y))
        .collect::<Vec<_>>()
        .join(" -> ");
    println!("{label}: {coords}");
}

fn main() -> Result<(), MatchError> {
    let network = road();

    // The simplest approach is to use the trait implementation, like so.
    // This is best on one-off or proof-of-concept use cases, when you don't
    // need to customize the costing, generator, or weighing strategy.
    let routed = network.match_simple(trace())?;
    println!(
        "facade: {} matched points, {} interpolated elements",
        routed.discretized.elements.len(),
        routed.interpolated.elements.len(),
    );

    // However, you can also build the Matcher yourself, to customize the
    // costing, generator, or weighing strategy. I.e., to cache predicate
    // results and avoid recomputing them for each match.
    let costing = CostingStrategies::default();
    let generator = StandardGenerator::new(&network, &costing.emission, DEFAULT_SEARCH_DISTANCE);
    let matcher = Matcher::new(&network, &costing, generator, AllCompute::default(), &());

    // Then, just use the "match" method to solve it, easy as!
    let collapsed = matcher.r#match(trace())?;
    for (candidate, r) in collapsed.matched().iter().zip(&collapsed.route) {
        println!(
            "  {r}: input point snapped to ({:.4}, {:.4}) on edge {:?}",
            candidate.position.x(),
            candidate.position.y(),
            candidate.edge.id(),
        );
    }

    // Collapsed is a linestring 1:1 with input positions.
    print_line("collapsed   ", &collapsed.collapsed());

    // Interpolated is the full path, including turn geometries, etc.
    print_line("interpolated", &collapsed.interpolated(&network));

    Ok(())
}
