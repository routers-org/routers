use geo::LineString;
use routers::r#match::MatchSimpleExt;
use routers_codec::osm::OsmNetwork;
use wkt::TryFromWkt;

use routers_fixtures::{SYDNEY, SYDNEY_SAVED, SYNDEY_TRIP, fixture};

fn main() {
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(SYNDEY_TRIP).expect("must parse");

    let pbf_path = fixture!(SYDNEY);
    let saved_path = fixture!(SYDNEY_SAVED);

    if !saved_path.exists() {
        let graph = OsmNetwork::from_pbf(pbf_path).expect("Graph must be created");
        graph.save_to_file(saved_path).expect("must save to file");
    }

    let graph = OsmNetwork::from_saved(saved_path).expect("Graph must be created");

    let route = graph
        .r#match_simple(coordinates.clone())
        .expect("Match must complete successfully");

    let linestring = route
        .discretized
        .iter()
        .map(|v| Point(v.point))
        .collect::<LineString<_>>();

    println!("Matched Route: {:?}", linestring);
}
