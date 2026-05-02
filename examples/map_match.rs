use geo::{LineString, Point};
use routers::r#match::MatchSimpleExt;
use routers_codec::osm::OsmNetwork;
use wkt::TryFromWkt;

use routers_fixtures::{SYDNEY, SYNDEY_TRIP, fixture};

fn main() {
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(SYNDEY_TRIP).expect("must parse");

    let pbf_path = fixture!(SYDNEY);
    let graph = OsmNetwork::from_pbf(pbf_path).expect("Graph must be created");

    let route = graph
        .r#match_simple(coordinates)
        .expect("Match must complete successfully");

    let linestring = route
        .discretized
        .iter()
        .map(|v| Point(v.point))
        .collect::<LineString<_>>();

    println!("Matched Route: {:?}", linestring);
}
