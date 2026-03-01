use geo::{LineString, Point};
use routers::r#match::MatchSimpleExt;
use routers_codec::osm::OsmNetwork;
use std::path::Path;
use wkt::TryFromWkt;

use routers_fixtures::{LOS_ANGELES, VENTURA_TRIP, fixture};

fn main() {
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(VENTURA_TRIP).expect("Linestring must parse successfully.");

    let path = Path::new(fixture!(LOS_ANGELES))
        .as_os_str()
        .to_ascii_lowercase();

    let graph = OsmNetwork::new(path).expect("Graph must be created");

    let route = graph
        .r#match_simple(coordinates)
        .expect("Match must complete successfully");

    let linestring = route
        .interpolated
        .iter()
        .map(|v| Point(v.point))
        .collect::<LineString<_>>();

    println!("Matched Route: {:?}", linestring);
}
