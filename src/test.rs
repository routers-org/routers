use routers_fixtures::{
    LAX_LYNWOOD_MATCHED, LAX_LYNWOOD_TRIP, LOS_ANGELES, VENTURA_MATCHED, VENTURA_TRIP, ZURICH,
    fixture,
};

use crate::{CostingStrategies, Graph, Match, PrecomputeForwardSolver};
use codec::Metadata;
use codec::osm::OsmEdgeMetadata;
use geo::LineString;
use std::path::Path;
use wkt::TryFromWkt;

fn assert_subsequence(a: &[i64], b: &[i64]) {
    let mut a_iter = a.iter();

    for b_item in b {
        if !a_iter.any(|a_item| a_item == b_item) {
            panic!(
                "b is not a subsequence of a: element {} not found in remaining portion of a",
                b_item
            );
        }
    }
}

#[test]
fn it_matches() {
    let source = LOS_ANGELES;
    let input_linestring = LAX_LYNWOOD_TRIP;
    let expected_linestring = LAX_LYNWOOD_MATCHED;

    let path = Path::new(fixture!(source)).as_os_str().to_ascii_lowercase();
    let graph = Graph::new(path).expect("Graph must be created");

    let runtime = OsmEdgeMetadata::runtime(None);

    // Yield the transition layers of each level
    // & Collapse the layers into a final vector
    let solver = PrecomputeForwardSolver::default();
    let coordinates: LineString<f64> = LineString::try_from_wkt_str(input_linestring)
        .expect("Linestring must parse successfully.");

    let result = graph
        .r#match(
            &runtime,
            solver.use_cache(graph.cache.clone()),
            coordinates.clone(),
        )
        .expect("Match must complete successfully");

    let edges = result
        .interpolated
        .iter()
        .map(|element| element.edge.id().identifier)
        .collect::<Vec<_>>();

    assert_subsequence(expected_linestring, &edges);
}
