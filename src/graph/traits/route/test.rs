use crate::graph::Route;
use crate::graph::traits::util::init_graph;
use geo::{LineString, Point, coord};
use routers_codec::Node;
use routers_codec::osm::OsmEntryId;
use routers_fixtures::DISTRICT_OF_COLUMBIA;
use std::error::Error;
use std::time::Instant;
use wkt::ToWkt;

fn generate_linestring(route: Vec<Node<OsmEntryId>>) -> String {
    route
        .iter()
        .map(|node| node.position)
        .collect::<LineString>()
        .wkt_string()
}

#[test]
fn columbia_mapping() -> Result<(), Box<dyn Error>> {
    let graph = init_graph(DISTRICT_OF_COLUMBIA)?;
    let time = Instant::now();

    let start = coord! { x: -77.02343850496823, y: 38.91261500917026 };
    let end = coord! { x: -77.03456230592386, y: 38.91772552535467 };

    let (weight, route) = graph
        .route_points(Point(start), Point(end))
        .expect("Could not produce route");

    println!("Took: {:?}", time.elapsed());

    println!("{}", generate_linestring(route));
    assert_eq!(weight, 216, "Incorrect Route Weighting");

    Ok(())
}

#[test]
#[cfg(any())]
fn stutgard_mapping() -> Result<(), RouteError> {
    let graph = init_graph(BADEN_WUERTTEMBERG)?;
    let start = coord! { x: 9.186777765, y: 48.773585361 };
    let end = coord! { x: 9.170598155, y: 48.779354943 };

    let time = Instant::now();
    let (weight, route) = graph
        .route(Point(start), Point(end))
        .expect("Could not produce route");

    println!("Took: {:?}", time.elapsed());

    println!("{}", generate_linestring(route));
    assert_eq!(weight, 658, "Incorrect Route Weighting");

    Ok(())
}

#[test]
#[cfg(any())]
fn sydney_mapping() -> Result<(), RouteError> {
    let graph = init_graph(SYDNEY)?;

    let start = coord! { x: 151.180025, y: -33.883572 };
    let end = coord! { x: 151.201438, y: -33.890029 };

    println!("Start: {:?}", graph.nearest_node(Point(start)));

    let time = Instant::now();
    let (weight, route) = graph
        .route(Point(start), Point(end))
        .expect("Could not produce route");

    println!("Took: {:?}", time.elapsed());

    println!("{}", generate_linestring(route));
    assert_eq!(weight, 450, "Incorrect Route Weighting");
    Ok(())
}
