//! Integration tests over the `sydney-overture` fixture: a real Overture
//! transportation extract (release `2026-08-19.0`) of the Sydney inner-west.

use geo::point;
use routers_codec::overture::{OvertureNetwork, RoadClass, reader};
use routers_fixtures::{SYDNEY_OVERTURE, fixture_path};
use routers_network::{Route, Scan};

fn sydney() -> OvertureNetwork {
    OvertureNetwork::from_geoparquet(&fixture_path(SYDNEY_OVERTURE))
        .expect("fixture builds a network")
}

#[test]
fn reads_real_extract() {
    let transportation =
        reader::read_transportation(&fixture_path(SYDNEY_OVERTURE)).expect("fixture reads");

    assert!(
        transportation.connectors.len() > 5_000,
        "expected thousands of connectors, got {}",
        transportation.connectors.len()
    );
    // The extract contains rail and footway segments; only road subtypes
    // survive the reader.
    assert!(
        transportation.segments.len() > 3_000,
        "expected thousands of road segments, got {}",
        transportation.segments.len()
    );
    assert!(
        transportation
            .segments
            .iter()
            .any(|s| s.road_class == Some(RoadClass::Primary)),
        "primary roads present in the Sydney extract"
    );
    assert!(
        transportation.segments.iter().any(|s| !s.access.is_empty()),
        "access restrictions parsed from real data"
    );
    assert!(
        transportation
            .segments
            .iter()
            .any(|s| s.speed_limits.iter().any(|l| l.max_speed.is_some())),
        "speed limits parsed from real data"
    );
}

#[test]
fn builds_routable_network() {
    let net = sydney();

    assert!(net.num_nodes() > 2_000, "graph nodes: {}", net.num_nodes());
    assert!(net.num_edges() > 4_000, "graph edges: {}", net.num_edges());

    // Two points from the SYDNEY_TRIP fixture, well inside the bbox.
    let start = point! { x: 151.194792, y: -33.88538 };
    let finish = point! { x: 151.184793, y: -33.90718 };

    let (weight, route) = net
        .route_points(&start, &finish)
        .expect("route across the inner-west");
    assert!(weight > 0);
    assert!(route.len() > 10, "route hops: {}", route.len());

    let nearest = net
        .nearest_node(&start)
        .expect("nearest node to trip start");
    let position = nearest.position;
    assert!((position.x() - start.x()).abs() < 0.01);
    assert!((position.y() - start.y()).abs() < 0.01);
}

#[test]
fn round_trips_real_network_through_bytes() {
    let net = sydney();
    let bytes = net.to_bytes().expect("serialise");
    let restored = OvertureNetwork::from_bytes(&bytes).expect("deserialise");

    assert_eq!(restored.num_nodes(), net.num_nodes());
    assert_eq!(restored.num_edges(), net.num_edges());
    assert_eq!(restored.segments().count(), net.segments().count());
}
