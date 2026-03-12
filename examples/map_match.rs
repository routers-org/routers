use geo::{LineString, Point};
use routers::r#match::MatchSimpleExt;
use routers_codec::osm::OsmNetwork;
use std::path::Path;
use wkt::TryFromWkt;

use routers_fixtures::{SYDNEY, fixture};

fn main() {
    let coordinates: LineString<f64> = LineString::try_from_wkt_str("LINESTRING (151.195157 -33.886921, 151.195822 -33.886529, 151.196101 -33.885522, 151.195704 -33.884952, 151.194717 -33.884979, 151.19447 -33.885701)")
        .expect("must parse");

    // let coordinates: LineString<f64> =
    // LineString::try_from_wkt_str(routers_fixtures::VENTURA_TRIP).expect("Linestring must parse successfully.");

    let path = Path::new(fixture!(SYDNEY)).as_os_str().to_ascii_lowercase();

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
