use geo::LineString;
use routers_fixtures::{LOS_ANGELES, fixture};
#[cfg(test)]
use std::sync::LazyLock;
use wkt::TryFromWkt;

use crate::{Graph, MatchSimpleExt, impls::osm::OsmGraph};

#[cfg(test)]
pub static MAP: LazyLock<OsmGraph> = LazyLock::new(|| {
    let path = std::path::Path::new(fixture!(LOS_ANGELES))
        .as_os_str()
        .to_ascii_lowercase();

    Graph::new(path).expect("must initialise")
});

fn setup(linestring: &str) -> LineString<f64> {
    // Yield the transition layers of each level
    // & Collapse the layers into a final vector
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(linestring).expect("Linestring must parse successfully.");

    coordinates
}

#[test]
fn ventura() {
    use routers_fixtures::VENTURA_TRIP;
    let coordinates = setup(VENTURA_TRIP);

    let result = MAP
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
