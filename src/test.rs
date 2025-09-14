use geo::LineString;
use routers_fixtures::fixture;
use std::path::Path;
use wkt::TryFromWkt;

use crate::{Graph, MatchSimpleExt, impls::osm::OsmGraph};

fn setup(source: &str, linestring: &str) -> (OsmGraph, LineString<f64>) {
    let path = Path::new(fixture!(source)).as_os_str().to_ascii_lowercase();
    let graph = Graph::new(path).expect("Graph must be created");

    // Yield the transition layers of each level
    // & Collapse the layers into a final vector
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(linestring).expect("Linestring must parse successfully.");

    (graph, coordinates)
}

#[test]
fn lax_lynwood() {
    use routers_fixtures::{LAX_LYNWOOD_TRIP, LOS_ANGELES};
    let (graph, coordinates) = setup(LOS_ANGELES, LAX_LYNWOOD_TRIP);

    let result = graph
        .match_simple(coordinates)
        .expect("Match must complete successfully");

    insta::assert_debug_snapshot!(result);
}
