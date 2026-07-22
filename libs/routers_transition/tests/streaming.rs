//! The realtime (streaming) lifecycle: `begin` → `push` → `solve` per tick,
//! `finish` at the end — asserted equivalent to the one-shot batch match, and
//! resumable across serialization.

use geo::{LineString, Point, point, wkt};
use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
use routers_transition::candidate::CollapsedPath;
use routers_transition::costing::{CostingStrategies, DefaultEmissionCost, DefaultTransitionCost};
use routers_transition::layer::generation::StandardGenerator;
use routers_transition::matcher::Trip;
use routers_transition::weigh::AllCompute;
use routers_transition::{Continuation, MatchError, Matcher};

type Costing = CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, MockEntryId>;

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

fn assert_same_match(a: &CollapsedPath<MockEntryId>, b: &CollapsedPath<MockEntryId>) {
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
    let generator = StandardGenerator::new(&net, &costing.emission);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    let batch = m.r#match(trajectory()).expect("batch match must succeed");

    let mut trip = m.begin();
    for point in trajectory().into_points() {
        m.push(&mut trip, point).expect("push must anchor");
        m.solve(&mut trip).expect("every prefix must solve");
    }
    let streamed = m.snapshot(&mut trip).expect("snapshot must succeed");

    assert_same_match(&streamed, &batch);
}

/// A trip serialized mid-stream and resumed in a "new process" (fresh matcher,
/// fresh caches) must complete to the same match.
#[test]
fn trip_serde_round_trip_resumes() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = || StandardGenerator::new(&net, &costing.emission);
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
    let streamed = m2.snapshot(&mut resumed).expect("snapshot must succeed");

    let batch = m.r#match(trajectory()).expect("batch match must succeed");
    assert_same_match(&streamed, &batch);
}

/// A point with no nearby road is rejected and leaves the trip untouched, so
/// the stream can drop it and continue.
#[test]
fn unanchored_push_leaves_trip_unchanged() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission);
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
    let generator = || StandardGenerator::new(&net, &costing.emission);

    let a = Matcher::new(&net, &costing, generator(), AllCompute::default(), &())
        .r#match(trajectory())
        .expect("match must succeed");
    let b = Matcher::new(&net, &costing, generator(), AllCompute::default(), &())
        .r#match(trajectory())
        .expect("match must succeed");

    assert_same_match(&a, &b);
}

/// Trimming a trip to its last `n` layers must leave a consistent,
/// re-solvable state whose solution equals a fresh batch match of the same
/// suffix — candidates re-stamped, trellis cut, resolved boundaries kept.
#[test]
fn tail_matches_batch_over_suffix() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = || StandardGenerator::new(&net, &costing.emission);
    let m = Matcher::new(&net, &costing, generator(), AllCompute::default(), &());

    let points = trajectory().into_points();

    let mut trip = m.begin();
    for &point in &points {
        m.push(&mut trip, point).expect("push must anchor");
    }
    m.solve(&mut trip).expect("full trip must solve");

    trip.tail(3);
    assert_eq!(trip.layers(), 3, "trip must hold exactly the suffix");
    assert_eq!(trip.points(), &points[3..], "origins must be the suffix");
    assert!(!trip.is_solved(), "a cut certificate must reopen");

    let streamed = m.snapshot(&mut trip).expect("trimmed trip must re-solve");
    let batch = m
        .r#match(LineString::from(points[3..].to_vec()))
        .expect("batch match must succeed");
    assert_same_match(&streamed, &batch);
}

/// `tail` is windowing, not surgery: asking for at least the current size
/// changes nothing, and asking for zero empties the trip.
#[test]
fn tail_bounds_are_noop_and_empty() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    let mut trip = m.begin();
    for point in trajectory().into_points() {
        m.push(&mut trip, point).expect("push must anchor");
    }
    m.solve(&mut trip).expect("trip must solve");

    trip.tail(usize::MAX);
    assert_eq!(trip.layers(), 6, "oversized tail must be a no-op");
    assert!(trip.is_solved(), "a no-op tail must keep the certificate");

    trip.tail(0);
    assert!(trip.is_empty(), "tail(0) must empty the trip");
}

/// A persisted trip whose origins overlap the committed history resumes:
/// trimmed to the overlap, with only the unseen points left to push — and the
/// resumed stream lands on the batch match of the history.
#[test]
fn reconcile_resumes_and_trims_to_overlap() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    let points = trajectory().into_points();

    // "Yesterday's" persisted trip: the first four points.
    let mut persisted = m.begin();
    for &point in &points[..4] {
        m.push(&mut persisted, point).expect("push must anchor");
    }
    m.solve(&mut persisted).expect("prefix must solve");

    // Today's committed history: the window slid past the first point and
    // two new points arrived.
    let history = &points[1..];

    let Continuation::Resume { mut trip, fresh } =
        Continuation::reconcile(Some(persisted), history)
    else {
        panic!("overlapping history must resume");
    };
    assert_eq!(trip.layers(), 3, "trip must trim to the overlap");
    assert_eq!(trip.points(), &points[1..4]);
    assert_eq!(fresh, points[4..].to_vec(), "unseen points must be fresh");

    for point in fresh {
        m.push(&mut trip, point).expect("push must anchor");
    }
    let streamed = m.snapshot(&mut trip).expect("resumed trip must solve");
    let batch = m
        .r#match(LineString::from(history.to_vec()))
        .expect("batch match must succeed");
    assert_same_match(&streamed, &batch);
}

/// A history the trip's origins never overlap (a teleport cut everything the
/// trellis knew) — and the absence of any trip at all — both restart.
#[test]
fn reconcile_restarts_on_divergence() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission);
    let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());

    let points = trajectory().into_points();

    let mut persisted = m.begin();
    for &point in &points[..3] {
        m.push(&mut persisted, point).expect("push must anchor");
    }

    // Post-teleport: the orchestrator discarded everything the trip has seen.
    let history = points[3..].to_vec();

    match Continuation::reconcile(Some(persisted), &history) {
        Continuation::Restart { fresh } => assert_eq!(fresh, history),
        Continuation::Resume { .. } => panic!("disjoint history must restart"),
    }

    match Continuation::<MockEntryId>::reconcile(None, &history) {
        Continuation::Restart { fresh } => assert_eq!(fresh, history),
        Continuation::Resume { .. } => panic!("no trip must restart"),
    }
}

/// `LayerId` indexes everything on a trip: origins, candidate layers, trellis.
#[test]
fn trip_accessors_are_layer_indexed() {
    let net = bent_road();
    let costing = Costing::default();
    let generator = StandardGenerator::new(&net, &costing.emission);
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
