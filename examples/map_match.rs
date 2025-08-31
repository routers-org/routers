use std::path::Path;
use geo::LineString;
use wkt::TryFromWkt;

use routers::*;
use routers_fixtures::{fixture, LOS_ANGELES, VENTURA_TRIP};

fn main() {
    let coordinates: LineString<f64> = LineString::try_from_wkt_str(VENTURA_TRIP)
        .expect("Linestring must parse successfully.");

    let path = Path::new(fixture!(LOS_ANGELES))
        .as_os_str()
        .to_ascii_lowercase();

    let graph = Graph::new(path).expect("Graph must be created");

    let route = graph
        .r#match_simple(coordinates)
        .expect("Match must complete successfully");

    println!("Matched Route: {route:?}");
}