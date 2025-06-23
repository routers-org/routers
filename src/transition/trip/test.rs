use crate::transition::Trip;
use routers_codec::osm::OsmEntryId;

use approx::assert_relative_eq;
use geo::wkt;
use routers_codec::primitive::Node;

const SHARED_DISTANCE: f64 = 900.0;

#[test]
fn test_trip() {
    use geo::Point;

    let nodes: Vec<Node<OsmEntryId>> = vec![
        Node::new(Point::new(0.0, 0.0), OsmEntryId::null()),
        Node::new(Point::new(0.0, 1.0), OsmEntryId::null()),
        Node::new(Point::new(1.0, 1.0), OsmEntryId::null()),
        Node::new(Point::new(1.0, 0.0), OsmEntryId::null()),
        Node::new(Point::new(1.0, -1.0), OsmEntryId::null()),
    ];

    let trip = Trip::from(nodes);

    let angles = trip.headings();
    assert_relative_eq!(angles[0], 0.0);
    assert_relative_eq!(angles[1], 90.0, max_relative = 1.0);
    assert_relative_eq!(angles[2], 180.0);
    assert_relative_eq!(angles[3], 180.0);

    assert_relative_eq!(trip.total_angle(), 180.0);
}

#[test]
fn validate_segment() {
    let linestring = wkt! {
        LINESTRING (-118.618033 34.166292, -118.623419 34.164641, -118.626895 34.163434)
    };

    let nodes = linestring
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()))
        .collect::<Vec<_>>();

    let trip = Trip::from(nodes);

    let angle = trip.total_angle();
    assert_relative_eq!(angle, 2.44, max_relative = 0.1);

    let imm_angle = trip.immediate_angle().abs();
    assert_relative_eq!(imm_angle, 0.81, max_relative = 0.1);

    let exp_angle = trip.angular_complexity(SHARED_DISTANCE);
    assert_relative_eq!(exp_angle, 0.95, max_relative = 0.1);
}

#[test]
fn validate_turning_path() {
    let linestring = wkt! {
        LINESTRING (-118.61829 34.166594, -118.623312 34.164996, -118.62329 34.164073, -118.624127 34.163896, -118.624449 34.163736, -118.625554 34.163461, -118.625929 34.163327, -118.626637 34.162928)
    };

    let nodes = linestring
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));

    let trip = Trip::new(nodes);

    let angle = trip.total_angle();
    assert_relative_eq!(angle, 195.30, max_relative = 0.1);

    let imm_angle = trip.immediate_angle().abs();
    assert_relative_eq!(imm_angle, 24.41, max_relative = 0.1);

    let exp_angle = trip.angular_complexity(SHARED_DISTANCE);
    assert_relative_eq!(exp_angle, 0.65, max_relative = 0.1);
}

#[test]
fn validate_uturn_expensive() {
    let linestring = wkt! {
        LINESTRING (-118.509833 34.170873, -118.505648 34.170891, -118.51406 34.170908, -118.509849 34.170926, -118.509865 34.172293)
    };

    let nodes = linestring
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));

    let trip = Trip::new(nodes);

    let length = trip.length();
    assert_relative_eq!(length, 1698.0, max_relative = 0.1);

    let angle = trip.total_angle();
    assert_relative_eq!(angle, 449.40, max_relative = 0.1);

    // Discouragingly complex
    let imm_angle = trip.angular_complexity(length);
    assert_relative_eq!(imm_angle, 0., max_relative = 0.1);
}

#[test]
fn validate_through_lower_cost() {
    let linestring_through_trip = wkt! {
        LINESTRING (-118.236761 33.945685, -118.236447 33.945696, -118.236341 33.945703, -118.23623 33.945723, -118.236133 33.945751, -118.236041 33.945797, -118.235908 33.945868, -118.235774 33.94596, -118.235509 33.946225, -118.235419 33.946304, -118.235322 33.946382, -118.235192 33.946447, -118.235031 33.946503, -118.234928 33.946525, -118.234797 33.946538, -118.234501 33.946542)
    };

    let linestring_around_trip = wkt! {
        LINESTRING (-118.236761 33.945685, -118.236759 33.946891, -118.236759 33.946891, -118.236758 33.947095, -118.235875 33.947112, -118.235546 33.947118, -118.235546 33.947118, -118.234899 33.947131, -118.234483 33.947139, -118.234501 33.946542)
    };

    let nodes = linestring_through_trip
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let through = Trip::new(nodes);

    let nodes = linestring_around_trip
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let around = Trip::new(nodes);

    const SHARED_DISTANCE: f64 = 227.;

    let imm_angle = around.angular_complexity(SHARED_DISTANCE);
    assert_relative_eq!(imm_angle, 0.333, max_relative = 0.1);

    let imm_angle = through.angular_complexity(SHARED_DISTANCE);
    assert_relative_eq!(imm_angle, 0.512, max_relative = 0.1);

    let len = around.length();
    assert_relative_eq!(len, 433.0, max_relative = 0.1);

    let len = through.length();
    assert_relative_eq!(len, 241.52, max_relative = 0.1);
}

#[test]
fn validate_slip_road_optimality() {
    use crate::transition::Trip;

    let linestring_sliproad = wkt! {
        LINESTRING (-118.138707 33.917051, -118.13859 33.917027, -118.138402 33.916998, -118.138172 33.916897, -118.138106 33.916837, -118.138078 33.916778, -118.138076 33.916697, -118.138251 33.916449, -118.138268 33.916424)
    };

    let linestring_around = wkt! {
        LINESTRING (-118.138707 33.917051, -118.13859 33.917027, -118.138402 33.916998, -118.138174 33.916984, -118.137992 33.916977, -118.137992 33.916977, -118.137881 33.916973, -118.138076 33.916697, -118.138251 33.916449, -118.138273 33.916417)
    };

    let nodes = linestring_sliproad
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let sliproad = Trip::new(nodes);

    let nodes = linestring_around
        .into_points()
        .into_iter()
        .map(|p| Node::new(p, OsmEntryId::null()));
    let around = Trip::new(nodes);

    const SHARED_DISTANCE: f64 = 90.;

    let tot_angle = sliproad.total_angle();
    assert_relative_eq!(tot_angle, 114.1, max_relative = 0.1);

    let tot_angle = around.total_angle();
    assert_relative_eq!(tot_angle, 129.97, max_relative = 0.1);

    let imm_angle = around.angular_complexity(SHARED_DISTANCE);
    assert_relative_eq!(imm_angle, 0.15, max_relative = 0.1);

    let imm_angle = sliproad.angular_complexity(SHARED_DISTANCE);
    assert_relative_eq!(imm_angle, 0.2, max_relative = 0.1);

    let len = around.length();
    assert_relative_eq!(len, 148.5, max_relative = 0.1);

    let len = sliproad.length();
    assert_relative_eq!(len, 113.0, max_relative = 0.1);
}
