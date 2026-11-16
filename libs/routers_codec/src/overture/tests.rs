//! Pure-logic tests for the Overture codec (no parquet / filesystem).
//!
//! These exercise the parser types, the segment→edge topology transform, the
//! metadata `accessible` check, `.rt` round-tripping and generic
//! `N: Network` drop-in compatibility. The GeoParquet reader is covered by
//! `reader::tests` (behind the `overture` feature).

use geo::{LineString, line_string, point};
use routers_network::{DataPlane, Direction, Discovery, Metadata, Route};

use crate::overture::element::{Connector, Segment, SegmentConnector};
use crate::overture::graph::OvertureNetwork;
use crate::overture::id::{Interner, OvertureEntryId};
use crate::overture::meta::OvertureEdgeMetadata;
use crate::overture::parsers::{
    AccessRestriction, AccessType, Between, Heading, RoadClass, Speed, SpeedUnit, TravelMode,
};

fn id(n: i64) -> OvertureEntryId {
    OvertureEntryId::new(n)
}

fn connector(n: i64, x: f64, y: f64) -> Connector {
    Connector::new(id(n), point! { x: x, y: y })
}

fn conn_ref(n: i64, at: f64) -> SegmentConnector {
    SegmentConnector { id: id(n), at }
}

/// A `primary` road segment (chord geometry) referencing the given connectors.
fn road_segment(
    seg: i64,
    connectors: Vec<SegmentConnector>,
    access: Vec<AccessRestriction>,
) -> Segment {
    Segment::new(
        id(seg),
        LineString::new(Vec::new()),
        connectors,
        Some(RoadClass::Primary),
        false,
        Vec::new(),
        access,
    )
}

// --- Parsers --------------------------------------------------------------

#[test]
fn road_class_parses_and_weights() {
    assert_eq!("motorway".parse(), Ok(RoadClass::Motorway));
    assert_eq!("living_street".parse(), Ok(RoadClass::LivingStreet));
    assert!("not_a_class".parse::<RoadClass>().is_err());

    // Motorway is cheaper (preferred) than residential.
    assert!(RoadClass::Motorway.weighting() < RoadClass::Residential.weighting());
}

#[test]
fn road_class_navigability() {
    assert!(RoadClass::Primary.navigable());
    assert!(RoadClass::Service.navigable());
    assert!(!RoadClass::Footway.navigable());
    assert!(!RoadClass::Cycleway.navigable());
    assert!(!RoadClass::Unknown.navigable());
}

#[test]
fn speed_unit_conversion() {
    assert_eq!(Speed::new(50, SpeedUnit::Kmh).in_kmh(), 50);
    // 60 mph = 96.56 km/h, rounded to 97.
    assert_eq!(Speed::new(60, SpeedUnit::Mph).in_kmh(), 97);
    assert_eq!("km/h".parse(), Ok(SpeedUnit::Kmh));
    assert_eq!("mph".parse(), Ok(SpeedUnit::Mph));
    assert!("furlongs".parse::<SpeedUnit>().is_err());
}

#[test]
fn speed_unit_aliases_and_display() {
    // Slash-free variants seen in the wild parse to the schema unit.
    assert_eq!("kmh".parse(), Ok(SpeedUnit::Kmh));
    assert_eq!("kph".parse(), Ok(SpeedUnit::Kmh));
    // Display round-trips the schema spelling.
    assert_eq!(SpeedUnit::Kmh.to_string(), "km/h");
    assert_eq!(SpeedUnit::Mph.to_string(), "mph");
}

#[test]
fn travel_mode_coverage_hierarchy() {
    use TravelMode::*;

    // Grouping modes cover the concrete modes beneath them.
    assert!(Vehicle.covers(Car));
    assert!(Vehicle.covers(Bicycle));
    assert!(!Vehicle.covers(Foot));
    assert!(MotorVehicle.covers(Hgv));
    assert!(!MotorVehicle.covers(Bicycle));
    assert!(!MotorVehicle.covers(Foot));

    // Concrete modes cover only themselves.
    assert!(Car.covers(Car));
    assert!(!Car.covers(Truck));
    assert!(!Bus.covers(Car));
}

#[test]
fn between_normalises_and_overlaps() {
    // Endpoint order is not schema-enforced; clamp and sort defensively.
    let reversed = Between::new(0.9, 0.2);
    assert_eq!((reversed.start, reversed.end), (0.2, 0.9));

    let clamped = Between::new(-0.5, 1.5);
    assert_eq!((clamped.start, clamped.end), (0.0, 1.0));

    assert!(Between::new(0.0, 0.5).overlaps(&Between::new(0.4, 1.0)));
    assert!(!Between::new(0.0, 0.4).overlaps(&Between::new(0.4, 1.0)));
}

#[test]
fn access_denies_driving_by_heading() {
    let denied_forward = AccessRestriction {
        access_type: AccessType::Denied,
        heading: Some(Heading::Forward),
        mode: Vec::new(),
        conditional: false,
    };
    assert!(denied_forward.denies_driving(Heading::Forward));
    assert!(!denied_forward.denies_driving(Heading::Backward));

    // A denial scoped to pedestrians does not make the road one-way for cars.
    let denied_foot = AccessRestriction {
        access_type: AccessType::Denied,
        heading: Some(Heading::Forward),
        mode: vec![TravelMode::Foot],
        conditional: false,
    };
    assert!(!denied_foot.denies_driving(Heading::Forward));
}

#[test]
fn conditional_denial_is_not_a_blanket_denial() {
    // e.g. "denied to vehicles over 4.2m": holds only under a condition the
    // graph cannot assume, so it neither closes a direction nor blocks
    // access for a default trip.
    let over_height = AccessRestriction {
        access_type: AccessType::Denied,
        heading: None,
        mode: Vec::new(),
        conditional: true,
    };
    assert!(!over_height.denies_driving(Heading::Forward));
    assert!(!over_height.denies_driving(Heading::Backward));

    let seg = road_segment(
        100,
        vec![conn_ref(1, 0.0), conn_ref(2, 1.0)],
        vec![over_height],
    );
    let meta = OvertureEdgeMetadata::pick(&seg);
    let runtime = OvertureEdgeMetadata::default_runtime();

    assert!(meta.accessible(&runtime, Direction::Outgoing));
    assert!(meta.accessible(&runtime, Direction::Incoming));
}

// --- Topology -------------------------------------------------------------

#[test]
fn bidirectional_segment_splits_into_edge_chain() {
    let connectors = vec![
        connector(1, 0.0, 0.0),
        connector(2, 1.0, 0.0),
        connector(3, 2.0, 0.0),
    ];
    let segments = vec![road_segment(
        100,
        vec![conn_ref(1, 0.0), conn_ref(2, 0.5), conn_ref(3, 1.0)],
        Vec::new(),
    )];

    let net = OvertureNetwork::from_elements(connectors, segments);

    assert_eq!(net.num_nodes(), 3);
    // Two undirected links → four directed edges.
    assert_eq!(net.num_edges(), 4);
    assert!(net.edge(&id(1), &id(2)).is_some());
    assert!(net.edge(&id(2), &id(1)).is_some());
    assert!(net.edge(&id(2), &id(3)).is_some());
}

#[test]
fn connectors_are_ordered_by_at() {
    // Provide connectors out of `at` order; the chain must still be 1-2-3.
    let seg = road_segment(
        100,
        vec![conn_ref(3, 1.0), conn_ref(1, 0.0), conn_ref(2, 0.5)],
        Vec::new(),
    );
    let ids: Vec<i64> = seg.connectors.iter().map(|c| c.id.identifier).collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn oneway_segment_only_emits_one_direction() {
    let connectors = vec![connector(1, 0.0, 0.0), connector(2, 1.0, 0.0)];
    let access = vec![AccessRestriction {
        access_type: AccessType::Denied,
        heading: Some(Heading::Forward),
        mode: Vec::new(),
        conditional: false,
    }];
    let segments = vec![road_segment(
        100,
        vec![conn_ref(1, 0.0), conn_ref(2, 1.0)],
        access,
    )];

    let net = OvertureNetwork::from_elements(connectors, segments);

    // Forward (1→2) is denied; only backward (2→1) survives.
    assert!(net.edge(&id(1), &id(2)).is_none());
    assert!(net.edge(&id(2), &id(1)).is_some());
    assert_eq!(net.num_edges(), 1);
}

#[test]
fn conditional_oneway_keeps_both_directions() {
    // A time-scoped directional denial (e.g. a peak-hour turn ban) must not
    // hard-close the direction at build time; per-trip evaluation belongs to
    // the runtime.
    let connectors = vec![connector(1, 0.0, 0.0), connector(2, 1.0, 0.0)];
    let access = vec![AccessRestriction {
        access_type: AccessType::Denied,
        heading: Some(Heading::Forward),
        mode: Vec::new(),
        conditional: true,
    }];
    let segments = vec![road_segment(
        100,
        vec![conn_ref(1, 0.0), conn_ref(2, 1.0)],
        access,
    )];

    let net = OvertureNetwork::from_elements(connectors, segments);

    assert!(net.edge(&id(1), &id(2)).is_some());
    assert!(net.edge(&id(2), &id(1)).is_some());
}

#[test]
fn interior_vertices_split_at_mid_segment_connector() {
    // Five evenly-spaced vertices with a connector halfway: each connector
    // pair receives only its own interior vertex, and the vertex coincident
    // with the mid connector is excluded from both.
    let seg = Segment::new(
        id(100),
        line_string![
            (x: 0.0, y: 0.0),
            (x: 0.25, y: 0.0),
            (x: 0.5, y: 0.0),
            (x: 0.75, y: 0.0),
            (x: 1.0, y: 0.0),
        ],
        vec![conn_ref(1, 0.0), conn_ref(2, 0.5), conn_ref(3, 1.0)],
        Some(RoadClass::Primary),
        false,
        Vec::new(),
        Vec::new(),
    );

    let first: Vec<f64> = seg.interior_vertices(0.0, 0.5).map(|c| c.x).collect();
    let second: Vec<f64> = seg.interior_vertices(0.5, 1.0).map(|c| c.x).collect();

    assert_eq!(first, vec![0.25]);
    assert_eq!(second, vec![0.75]);
}

#[test]
fn interior_vertices_of_empty_geometry_are_none() {
    let seg = road_segment(100, vec![conn_ref(1, 0.0), conn_ref(2, 1.0)], Vec::new());
    assert_eq!(seg.interior_vertices(0.0, 1.0).count(), 0);
}

#[test]
fn interior_geometry_becomes_shape_nodes() {
    // A bent road: the linestring has a vertex halfway that is not a
    // connector. It must materialise as a synthetic node so the edge chain
    // follows the bend.
    let connectors = vec![connector(1, 0.0, 0.0), connector(2, 1.0, 0.0)];
    let bent = Segment::new(
        id(100),
        line_string![
            (x: 0.0, y: 0.0),
            (x: 0.5, y: 0.2),
            (x: 1.0, y: 0.0),
        ],
        vec![conn_ref(1, 0.0), conn_ref(2, 1.0)],
        Some(RoadClass::Primary),
        false,
        Vec::new(),
        Vec::new(),
    );
    let net = OvertureNetwork::from_elements(connectors, vec![bent]);

    // Two connectors plus one synthetic shape node; two hops each way.
    assert_eq!(net.num_nodes(), 3);
    assert_eq!(net.num_edges(), 4);
    assert!(net.edge(&id(1), &id(2)).is_none(), "no corner-cut chord");

    let synthetic = net
        .nodes()
        .find(|n| n.id != id(1) && n.id != id(2))
        .expect("synthetic shape node present");
    assert_eq!(synthetic.position, point! { x: 0.5, y: 0.2 });
}

#[test]
fn non_navigable_segments_are_dropped() {
    let connectors = vec![connector(1, 0.0, 0.0), connector(2, 1.0, 0.0)];
    let footway = Segment::new(
        id(100),
        LineString::new(Vec::new()),
        vec![conn_ref(1, 0.0), conn_ref(2, 1.0)],
        Some(RoadClass::Footway),
        false,
        Vec::new(),
        Vec::new(),
    );
    let net = OvertureNetwork::from_elements(connectors, vec![footway]);

    assert_eq!(net.num_edges(), 0);
    assert!(net.metadata(&id(100)).is_none());
}

#[test]
fn dangling_connector_reference_is_pruned() {
    // Segment 100 references connector 9, which has no connector feature;
    // segment 200 is fully resolvable.
    let connectors = vec![connector(1, 0.0, 0.0), connector(2, 1.0, 0.0)];
    let segments = vec![
        road_segment(100, vec![conn_ref(1, 0.0), conn_ref(9, 1.0)], Vec::new()),
        road_segment(200, vec![conn_ref(1, 0.0), conn_ref(2, 1.0)], Vec::new()),
    ];
    let net = OvertureNetwork::from_elements(connectors, segments);

    // The dangling pair is dropped entirely: every graph node must carry a
    // position (routing consumers rely on this invariant).
    assert!(net.edge(&id(1), &id(9)).is_none());
    assert!(net.edge(&id(1), &id(2)).is_some());
    assert!(
        net.edges()
            .all(|(s, t, _)| net.node(&s).is_some() && net.node(&t).is_some())
    );
}

#[test]
fn metadata_is_keyed_by_segment_id() {
    let connectors = vec![connector(1, 0.0, 0.0), connector(2, 1.0, 0.0)];
    let segments = vec![road_segment(
        100,
        vec![conn_ref(1, 0.0), conn_ref(2, 1.0)],
        Vec::new(),
    )];
    let net = OvertureNetwork::from_elements(connectors, segments);

    let meta = net.metadata(&id(100)).expect("segment metadata present");
    assert_eq!(meta.road_class, Some(RoadClass::Primary));
    assert!(meta.lane_count.is_none(), "overture has no lane data");
}

// --- Metadata accessibility ----------------------------------------------

#[test]
fn accessible_respects_directional_denial() {
    let seg = road_segment(
        100,
        vec![conn_ref(1, 0.0), conn_ref(2, 1.0)],
        vec![AccessRestriction {
            access_type: AccessType::Denied,
            heading: Some(Heading::Forward),
            mode: Vec::new(),
            conditional: false,
        }],
    );
    let meta = OvertureEdgeMetadata::pick(&seg);
    let runtime = OvertureEdgeMetadata::default_runtime();

    assert!(!meta.accessible(&runtime, Direction::Outgoing));
    assert!(meta.accessible(&runtime, Direction::Incoming));
}

// --- Serialisation + generics --------------------------------------------

fn sample_network() -> OvertureNetwork {
    let connectors = vec![
        connector(1, 0.0, 0.0),
        connector(2, 1.0, 0.0),
        connector(3, 2.0, 0.0),
    ];
    let segments = vec![road_segment(
        100,
        vec![conn_ref(1, 0.0), conn_ref(2, 0.5), conn_ref(3, 1.0)],
        Vec::new(),
    )];
    OvertureNetwork::from_elements(connectors, segments)
}

#[test]
fn round_trips_through_bytes() {
    let net = sample_network();
    let bytes = net.to_bytes().expect("serialise");
    let restored = OvertureNetwork::from_bytes(&bytes).expect("deserialise");

    assert_eq!(restored.num_nodes(), net.num_nodes());
    assert_eq!(restored.num_edges(), net.num_edges());
    assert!(restored.metadata(&id(100)).is_some());
}

#[test]
fn usable_as_generic_network() {
    // Compiles only because OvertureNetwork implements the full trait
    // stack — the drop-in guarantee for generic `N: Network` consumers.
    fn route_on<N: Route>(net: &N, from: N::Entry, to: N::Entry) -> Option<u32> {
        net.route_nodes(from, to).map(|(weight, _)| weight)
    }

    let net = sample_network();
    let weight = route_on(&net, id(1), id(3)).expect("route 1→3 exists");
    // primary + primary = 5 + 5.
    assert_eq!(weight, 10);
}

#[test]
fn interner_is_stable_and_collision_free() {
    let mut interner = Interner::new();
    let a = interner.intern("overture:transportation:connector:aaa");
    let b = interner.intern("overture:transportation:connector:bbb");
    let a_again = interner.intern("overture:transportation:connector:aaa");

    assert_eq!(a, a_again);
    assert_ne!(a, b);
    assert!(!a.is_null());
    assert_eq!(interner.len(), 2);
}
