use std::time::Instant;

use geo::{LineString, Point};
use routers::{MatchSimpleExt, codec::osm::OsmNetwork};
use wkt::TryFromWkt;

use routers_fixtures::{SYDNEY, SYDNEY_SAVED, SYNDEY_TRIP, fixture};

fn main() {
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(SYNDEY_TRIP).expect("must parse");

    let now = Instant::now();

    let pbf_path = fixture!(SYDNEY);
    let save_path = fixture!(SYDNEY_SAVED);

    let graph = OsmNetwork::from_pbf_and_save(pbf_path, save_path).expect("Graph must be created");
    println!("Starting, ingest took: {:?}", now.elapsed());

    let route = graph
        .match_simple(coordinates)
        .expect("Match must complete successfully");

    let linestring = route
        .discretized
        .iter()
        .map(|v| Point(v.point))
        .collect::<LineString<_>>();

    println!("Matched Route: {:?}", linestring);
    println!("Time taken: {:?}", now.elapsed());
}
