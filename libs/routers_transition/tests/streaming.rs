//! The realtime (streaming) lifecycle: `begin` → `push` → `solve` per tick,
//! `finish` at the end — asserted equivalent to the one-shot batch match, and
//! resumable across serialization.

use geo::{LineString, Point, point, wkt};
use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
use routers_transition::generation::StandardGenerator;
use routers_transition::{
    AllCompute, CostingStrategies, DEFAULT_SEARCH_DISTANCE, MatchError, Matcher, Trip,
};

type Costing = CostingStrategies<
    routers_transition::DefaultEmissionCost,
    routers_transition::DefaultTransitionCost,
    MockEntryId,
>;

/// A staircase road: west, then south, then west again.
fn bent_road() -> MockNetwork {
    MockNetworkBuilder::new()
        .node(1, point!(x: -118.15, y: 34.15))
        .node(2, point!(x: -118.16, y: 34.15))
        .node(3, point!(x: -118.17, y: 34.15))
        .node(4, point!(x: -118.17, y: 34.14))
        .node(5, point!(x: -118.18, y: 34.14))
        .edge(1, 2)
        .edge(2, 3)
        .edge(3, 4)
        .edge(4, 5)
        .build()
}

fn trajectory() -> LineString {
    wkt! {
        LINESTRING(
            -118.151 34.1503, -118.155 34.1503, -118.165 34.1503,
            -118.170 34.1490, -118.172 34.1403, -118.179 34.1403
        )
    }
}

fn assert_same_match(
    a: &routers_transition::CollapsedPath<MockEntryId>,
    b: &routers_transition::CollapsedPath<MockEntryId>,
) {
    assert_eq!(a.cost, b.cost, "costs must agree");
    assert_eq!(a.route, b.route, "chosen candidates must agree");
    assert_eq!(a.collapsed(), b.collapsed(), "matched positions must agree");
    assert_eq!(
        a.interpolated.len(),
        b.interpolated.len(),
        "hop counts must agree"
    );
    for (x, y) in a.interpolated.iter().zip(&b.interpolated) {
        assert_eq!(x.path, y.path, "re-derived hop geometry must agree");
    }
}

/// Pushing and re-solving one position at a time must land on exactly the
/// match the batch pipeline finds.
#[test]
fn streaming_equals_batch() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    let batch = m.r#match(trajectory()).expect("batch match must succeed");

    let mut trip = m.begin();
    for point in trajectory().into_points() {
        m.push(&mut trip, point).expect("push must anchor");
        m.solve(&mut trip).expect("every prefix must solve");
    }
    let streamed = m.finish(trip).expect("finish must succeed");

    assert_same_match(&streamed, &batch);
}

/// A trip serialized mid-stream and resumed in a "new process" (fresh matcher,
/// fresh caches) must complete to the same match.
#[test]
fn trip_serde_round_trip_resumes() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = || StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
    let m = Matcher::new(&net, &costing, generator(), AllCompute::default(), &());

    let points = trajectory().into_points();
    let (head, tail) = points.split_at(3);

    let mut trip = m.begin();
    for &point in head {
        m.push(&mut trip, point).expect("push must anchor");
    }
    m.solve(&mut trip).expect("prefix must solve");

    // Tick boundary: persist, drop everything, resume elsewhere.
    let stored = serde_json::to_string(&trip).expect("trip must serialize");
    drop(trip);

    let mut resumed: Trip<MockEntryId> =
        serde_json::from_str(&stored).expect("trip must deserialize");
    assert!(resumed.is_solved(), "solved state must survive the trip");

    let m2 = Matcher::new(&net, &costing, generator(), AllCompute::default(), &());
    for &point in tail {
        m2.push(&mut resumed, point).expect("push must anchor");
        m2.solve(&mut resumed).expect("every prefix must solve");
    }
    let streamed = m2.finish(resumed).expect("finish must succeed");

    let batch = m.r#match(trajectory()).expect("batch match must succeed");
    assert_same_match(&streamed, &batch);
}

/// A point with no nearby road is rejected and leaves the trip untouched, so
/// the stream can drop it and continue.
#[test]
fn unanchored_push_leaves_trip_unchanged() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    let mut trip = m.begin();
    m.push(&mut trip, point!(x: -118.151, y: 34.1503))
        .expect("on-road point must anchor");

    let off_network = point!(x: 0.0, y: 0.0);
    let err = m.push(&mut trip, off_network).expect_err("must reject");
    assert!(matches!(err, MatchError::Unanchored(_)));
    assert_eq!(trip.layers(), 1, "rejected push must not grow the trip");

    m.push(&mut trip, point!(x: -118.155, y: 34.1503))
        .expect("stream continues after a dropped point");
    let path = m.solve(&mut trip).expect("solve must succeed");
    assert_eq!(path.nodes.len(), 2);
}

/// Matching is deterministic: identical inputs give identical outputs, and the
/// collapse-time geometry re-derivation reproduces itself run over run.
#[test]
fn repeated_matches_reproduce_geometry() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = || StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);

    let a = Matcher::new(&net, &costing, generator(), AllCompute::default(), &())
        .r#match(trajectory())
        .expect("match must succeed");
    let b = Matcher::new(&net, &costing, generator(), AllCompute::default(), &())
        .r#match(trajectory())
        .expect("match must succeed");

    assert_same_match(&a, &b);
}

/// `LayerId` indexes everything on a trip: origins, candidate layers, trellis.
#[test]
fn trip_accessors_are_layer_indexed() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    let mut trip = m.begin();
    assert!(trip.is_empty() && trip.last_id().is_none());

    let origin: Point = point!(x: -118.151, y: 34.1503);
    let id = m.push(&mut trip, origin).expect("push must anchor");

    assert_eq!(trip.last_id(), Some(id));
    assert_eq!(trip.point(id), Some(origin));

    let layer = trip.layer(id).expect("layer must exist");
    assert!(!layer.is_empty());
    for (n, candidate) in layer.iter().enumerate() {
        assert_eq!(candidate.location.layer, id);
        assert_eq!(candidate.location.node.index(), n);
        assert_eq!(
            trip.candidate(&candidate.location).map(|c| c.position),
            Some(candidate.position)
        );
    }

    let trellis = trip.trellis().expect("trellis exists after first layer");
    assert_eq!(trellis.widths(), &[layer.len() as u32]);
}
