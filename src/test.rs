use geo::LineString;
use routers_codec::osm::OsmNetwork;
use routers_fixtures::fixture;
use wkt::TryFromWkt;

use crate::r#match::MatchSimpleExt;

fn setup(source: &str, linestring: &str) -> (OsmNetwork, LineString<f64>) {
    let graph = OsmNetwork::from_pbf(fixture!(source)).expect("Graph must be created");

    // Yield the transition layers of each level
    // & Collapse the layers into a final vector
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(linestring).expect("Linestring must parse successfully.");

    (graph, coordinates)
}

fn ventura() {
    use routers_fixtures::{LOS_ANGELES, VENTURA_TRIP};
    let (graph, coordinates) = setup(LOS_ANGELES, VENTURA_TRIP);

    let result = graph
        .match_simple(coordinates)
        .expect("Match must complete successfully");

    insta::assert_ron_snapshot!(
        result.interpolated.elements,
        {
             ".**.x" => insta::rounded_redaction(6),
             ".**.y" => insta::rounded_redaction(6)
        }
    );
}

fn lax_lynwood() {
    use routers_fixtures::{LAX_LYNWOOD_TRIP, LOS_ANGELES};
    let (graph, coordinates) = setup(LOS_ANGELES, LAX_LYNWOOD_TRIP);

    let result = graph
        .match_simple(coordinates)
        .expect("Match must complete successfully");

    insta::assert_ron_snapshot!(
        result.interpolated.elements,
        {
             ".**.x" => insta::rounded_redaction(6),
             ".**.y" => insta::rounded_redaction(6)
        }
    );
}
