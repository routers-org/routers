use geo::LineString;
use std::path::Path;
use wkt::TryFromWkt;

use routers::*;
use routers_fixtures::{LOS_ANGELES, VENTURA_TRIP, fixture};

fn main() {
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(VENTURA_TRIP).expect("Linestring must parse successfully.");

    let path = Path::new(fixture!(LOS_ANGELES))
        .as_os_str()
        .to_ascii_lowercase();

    let graph = Graph::new(path).expect("Graph must be created");

    let route = graph
        .r#match_simple(coordinates)
        .expect("Match must complete successfully");

    let linestring = route
        .discretized
        .iter()
        .map(|v| v.point)
        .collect::<LineString<_>>();

    println!("Matched Route: {:?}", linestring);
}
