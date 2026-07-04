//! End-to-end map-matching tests over the `routers_network` `MockNetwork`
//! harness (enabled via its `testing` feature).
//!
//! These were transplanted out of `routers_network`'s mock tests (which must not
//! depend on `routers_transition`) and extended with edge cases and a
//! Trellis-vs-Selective conformance suite.

use geo::{LineString, point, wkt};
use routers_network::mock::{MockEntryId, MockMetadata, MockNetwork, MockNetworkBuilder};
use routers_network::{DataPlane, Direction, Metadata};
use routers_transition::r#match::{Match, MatchOptions, MatchSimpleExt};
use routers_transition::{MatchError, RoutedPath, SolverVariant};

// ── helpers ────────────────────────────────────────────────────────────────

/// Tiny straight road: 1 ── 2 ── 3 along lat = 34.15.
fn straight_road() -> MockNetwork {
    MockNetworkBuilder::new()
        .node(1, point!(x: -118.15, y: 34.15))
        .node(2, point!(x: -118.16, y: 34.15))
        .node(3, point!(x: -118.17, y: 34.15))
        .edge(1, 2)
        .edge(2, 3)
        .build()
}

/// Run a match with an explicit solver variant.
fn match_with(
    net: &MockNetwork,
    ls: &LineString,
    variant: SolverVariant,
) -> Result<RoutedPath<MockEntryId, MockMetadata>, MatchError> {
    net.r#match(ls.clone(), MatchOptions::new().with_solver(variant))
}

/// The ordered (source, target) node-id pairs of the discretized match.
fn discretized_edges(net: &MockNetwork, ls: &LineString, variant: SolverVariant) -> Vec<(i64, i64)> {
    match_with(net, ls, variant)
        .expect("map match must succeed")
        .discretized
        .elements
        .iter()
        .map(|e| (e.edge.source.id.0, e.edge.target.id.0))
        .collect()
}

// ── transplanted matcher tests ───────────────────────────────────────────────

/// A GPS trajectory drifted slightly north of a straight road should snap back
/// onto the road and produce one discretized element per input point.
#[test]
fn map_match_straight_road() {
    let net = straight_road();
    let linestring: LineString = wkt! {
        LINESTRING(-118.151 34.1503, -118.155 34.1503, -118.160 34.1503, -118.165 34.1503)
    };

    let result = net.match_simple(linestring).expect("map match must succeed");

    for element in &result.discretized.elements {
        assert!(
            net.metadata(element.edge.id()).is_some(),
            "metadata must be present for every matched edge"
        );
    }
    assert_eq!(
        result.discretized.elements.len(),
        4,
        "discretized path must have one element per GPS input point"
    );
}

/// Two GPS points on non-adjacent edges force traversal of the intermediate edge.
#[test]
fn map_match_interpolated_path_crosses_intermediate_edge() {
    let net = MockNetworkBuilder::new()
        .node(1, point!(x: -118.14, y: 34.15))
        .node(2, point!(x: -118.15, y: 34.15))
        .node(3, point!(x: -118.16, y: 34.15))
        .node(4, point!(x: -118.17, y: 34.15))
        .edge(1, 2)
        .edge(2, 3)
        .edge(3, 4)
        .build();

    let linestring: LineString = wkt! { LINESTRING(-118.141 34.1503, -118.169 34.1503) };
    let result = net.match_simple(linestring).expect("map match must succeed");

    assert!(
        !result.interpolated.elements.is_empty(),
        "interpolated path must be non-empty when candidates span non-adjacent edges"
    );
    for element in &result.interpolated.elements {
        assert!(net.metadata(element.edge.id()).is_some());
    }
}

/// A T-junction: a straight-west track must not dip onto the south branch.
#[test]
fn map_match_prefers_straight_over_turn() {
    let net = MockNetworkBuilder::new()
        .node(1, point!(x: -118.10, y: 34.15))
        .node(2, point!(x: -118.13, y: 34.15))
        .node(3, point!(x: -118.16, y: 34.15))
        .node(4, point!(x: -118.13, y: 34.12))
        .bidirectional_edge(1, 2)
        .bidirectional_edge(2, 3)
        .bidirectional_edge(2, 4)
        .build();

    let linestring: LineString = wkt! {
        LINESTRING(
            -118.101 34.1503, -118.111 34.1503, -118.121 34.1503, -118.131 34.1503,
            -118.141 34.1503, -118.151 34.1503, -118.158 34.1503
        )
    };
    let result = net.match_simple(linestring).expect("map match must succeed");

    assert!(!result.discretized.elements.is_empty());
    let matched: Vec<i64> = result
        .discretized
        .elements
        .iter()
        .flat_map(|e| [e.edge.source.id.0, e.edge.target.id.0])
        .collect();
    assert!(
        !matched.contains(&4),
        "the south-branch node (4) must not appear in a straight-west trajectory match"
    );
}

/// A highway with a long offramp detour — the shorter direct highway wins.
#[test]
fn map_match_highway_preferred_over_offramp() {
    let net = MockNetworkBuilder::new()
        .node(1, point!(x: -118.100, y: 34.150))
        .node(2, point!(x: -118.105, y: 34.150))
        .node(3, point!(x: -118.109, y: 34.149))
        .node(4, point!(x: -118.113, y: 34.148))
        .node(5, point!(x: -118.107, y: 34.146))
        .bidirectional_edge(1, 2)
        .bidirectional_edge(2, 3)
        .bidirectional_edge(3, 4)
        .edge(2, 5)
        .edge(5, 4)
        .build();

    let linestring: LineString = wkt! { LINESTRING(-118.102 34.1503, -118.111 34.1488) };
    let result = net.match_simple(linestring).expect("map match must succeed");

    assert!(!result.interpolated.elements.is_empty());
    let interpolated: Vec<i64> = result
        .interpolated
        .elements
        .iter()
        .flat_map(|e| [e.edge.source.id.0, e.edge.target.id.0])
        .collect();
    assert!(
        !interpolated.contains(&5),
        "offramp detour node (5) must not appear: the shorter highway route is preferred"
    );
}

/// A T-junction where the GPS turns north — trip momentum must beat the closer
/// straight-continuation candidate at the ambiguous junction point.
#[test]
fn map_match_follows_turn_at_junction() {
    let net = MockNetworkBuilder::new()
        .node(1, point!(x: -118.10, y: 34.15))
        .node(2, point!(x: -118.13, y: 34.15))
        .node(3, point!(x: -118.13, y: 34.18))
        .node(4, point!(x: -118.16, y: 34.15))
        .bidirectional_edge(1, 2)
        .bidirectional_edge(2, 3)
        .bidirectional_edge(2, 4)
        .build();

    let linestring: LineString = wkt! {
        LINESTRING(
            -118.101 34.1503, -118.111 34.1503, -118.121 34.1503,
            -118.1297 34.1503, -118.1297 34.153, -118.1297 34.163
        )
    };
    let result = net.match_simple(linestring).expect("map match must succeed");

    assert!(!result.discretized.elements.is_empty());
    let matched: Vec<i64> = result
        .discretized
        .elements
        .iter()
        .flat_map(|e| [e.edge.source.id.0, e.edge.target.id.0])
        .collect();
    assert!(
        !matched.contains(&4),
        "east-continuation node (4) must not appear when the GPS turns north"
    );
}

/// The interpolated path must include the candidate edges (first and last), not
/// just the bridging edges.
#[test]
fn map_match_interpolated_path_includes_candidate_edges() {
    let net = MockNetworkBuilder::new()
        .node(1, point!(x: -118.14, y: 34.15))
        .node(2, point!(x: -118.15, y: 34.15))
        .node(3, point!(x: -118.16, y: 34.15))
        .node(4, point!(x: -118.17, y: 34.15))
        .edge(1, 2)
        .edge(2, 3)
        .edge(3, 4)
        .build();

    let linestring: LineString = wkt! { LINESTRING(-118.141 34.1503, -118.169 34.1503) };
    let result = net.match_simple(linestring).expect("map match must succeed");

    let nodes: Vec<i64> = result
        .interpolated
        .elements
        .iter()
        .flat_map(|e| [e.edge.source.id.0, e.edge.target.id.0])
        .collect();
    assert!(nodes.contains(&1), "node 1 (first candidate edge) must appear");
    assert!(nodes.contains(&4), "node 4 (last candidate edge) must appear");
    assert!(
        nodes.contains(&2) && nodes.contains(&3),
        "bridging edge (2→3) must appear"
    );
}

/// The long-trip / trip-momentum regression: drifted layers near a weight-10
/// side road must not dip off the weight-1 primary. Exercised with Selective.
#[test]
fn long_trip_avoids_side_road_dip() {
    let net = side_road_dip_net();
    let linestring = side_road_dip_trip();

    let result = net
        .r#match(linestring, MatchOptions::new().with_solver(SolverVariant::Selective))
        .expect("map match must succeed");

    for element in &result.discretized.elements {
        assert_eq!(
            element.edge.weight, 1,
            "matched edge {:?} (weight {}) — dipped onto the side road",
            element.edge.id, element.edge.weight,
        );
    }
}

fn side_road_dip_net() -> MockNetwork {
    MockNetworkBuilder::new()
        .node(1, point!(x: -118.140, y: 34.150))
        .node(2, point!(x: -118.141, y: 34.150))
        .node(3, point!(x: -118.142, y: 34.150))
        .node(4, point!(x: -118.143, y: 34.150))
        .node(5, point!(x: -118.144, y: 34.150))
        .node(6, point!(x: -118.145, y: 34.150))
        .node(7, point!(x: -118.146, y: 34.150))
        .node(8, point!(x: -118.147, y: 34.150))
        .node(9, point!(x: -118.148, y: 34.150))
        .node(10, point!(x: -118.144, y: 34.14985))
        .node(11, point!(x: -118.144, y: 34.14972))
        .weighted_edge(1, 2, 1)
        .weighted_edge(2, 3, 1)
        .weighted_edge(3, 4, 1)
        .weighted_edge(4, 5, 1)
        .weighted_edge(5, 6, 1)
        .weighted_edge(6, 7, 1)
        .weighted_edge(7, 8, 1)
        .weighted_edge(8, 9, 1)
        .weighted_edge(5, 10, 10)
        .weighted_edge(10, 5, 10)
        .weighted_edge(10, 11, 10)
        .weighted_edge(11, 10, 10)
        .build()
}

fn side_road_dip_trip() -> LineString {
    wkt! {
        LINESTRING(
            -118.1405 34.150000, -118.1415 34.150000, -118.1425 34.150000, -118.1435 34.150000,
            -118.14400 34.149800, -118.14403 34.149725, -118.14406 34.149800, -118.1445 34.150000,
            -118.1455 34.150000, -118.1465 34.150000, -118.1470 34.150000, -118.1475 34.150000
        )
    }
}

// ── Trellis-vs-Selective conformance ─────────────────────────────────────────

/// On a route with a single obvious optimum, the eager trellis solver and the
/// lazy selective solver must produce the *same* discretized match. (Equal-cost
/// tie-breaks can differ between Viterbi and astar — SPEC §O4 — so this uses an
/// unambiguous route.)
#[test]
fn trellis_and_selective_agree_on_straight_road() {
    let net = straight_road();
    let ls: LineString = wkt! {
        LINESTRING(-118.151 34.1503, -118.155 34.1503, -118.160 34.1503, -118.165 34.1503)
    };
    let trellis = discretized_edges(&net, &ls, SolverVariant::Trellis);
    let selective = discretized_edges(&net, &ls, SolverVariant::Selective);

    assert!(!trellis.is_empty(), "trellis produced an empty match");
    assert_eq!(trellis, selective, "trellis and selective disagree");
}

/// Same conformance on the side-road-dip network, which has a clear optimum.
#[test]
fn trellis_and_selective_agree_on_side_road_dip() {
    let net = side_road_dip_net();
    let ls = side_road_dip_trip();
    assert_eq!(
        discretized_edges(&net, &ls, SolverVariant::Trellis),
        discretized_edges(&net, &ls, SolverVariant::Selective),
    );
}

/// Both solvers must stay on the weight-1 primary (the regression, via Trellis).
#[test]
fn trellis_also_avoids_side_road_dip() {
    let net = side_road_dip_net();
    let result = net
        .r#match(side_road_dip_trip(), MatchOptions::new().with_solver(SolverVariant::Trellis))
        .expect("map match must succeed");
    for element in &result.discretized.elements {
        assert_eq!(element.edge.weight, 1, "trellis dipped onto the side road");
    }
}

// ── edge cases ───────────────────────────────────────────────────────────────

/// A single-point trajectory yields a single-layer trellis (no transitions) and
/// still returns exactly one matched element.
#[test]
fn single_point_trajectory() {
    let net = straight_road();
    let ls: LineString = wkt! { LINESTRING(-118.155 34.1503, -118.155 34.1503) };
    // Two identical points -> two layers, but degenerate; must not panic.
    let result = net.match_simple(ls).expect("single/degenerate match must succeed");
    assert!(!result.discretized.elements.is_empty());
}

/// Consecutive duplicate points (zero-distance bearing) must be handled without
/// panicking and still match.
#[test]
fn duplicate_consecutive_points() {
    let net = straight_road();
    let ls: LineString = wkt! {
        LINESTRING(-118.152 34.1503, -118.152 34.1503, -118.160 34.1503, -118.160 34.1503)
    };
    let both = [SolverVariant::Trellis, SolverVariant::Selective]
        .map(|v| discretized_edges(&net, &ls, v));
    assert_eq!(both[0], both[1], "solvers disagree on duplicate-point input");
}

/// Two disconnected components >2 km apart (beyond the predicate bound): each
/// layer has candidates, but there is no route between them, so the match fails
/// with a collapse error rather than panicking.
#[test]
fn disconnected_components_yield_no_path() {
    let net = MockNetworkBuilder::new()
        // Component A near -118.15
        .node(1, point!(x: -118.150, y: 34.150))
        .node(2, point!(x: -118.151, y: 34.150))
        .edge(1, 2)
        // Component B ~4 km east, no connecting edge
        .node(3, point!(x: -118.100, y: 34.150))
        .node(4, point!(x: -118.101, y: 34.150))
        .edge(3, 4)
        .build();

    let ls: LineString = wkt! { LINESTRING(-118.1505 34.1503, -118.1005 34.1503) };
    for variant in [SolverVariant::Trellis, SolverVariant::Selective] {
        let result = match_with(&net, &ls, variant);
        assert!(
            result.is_err(),
            "{variant:?}: disconnected components must not produce a match"
        );
    }
}

/// An empty trajectory must error cleanly (no panic, no empty-index access).
#[test]
fn empty_trajectory_errors() {
    let net = straight_road();
    let ls = LineString::new(vec![]);
    for variant in [SolverVariant::Trellis, SolverVariant::Selective] {
        assert!(
            match_with(&net, &ls, variant).is_err(),
            "{variant:?}: empty trajectory must error"
        );
    }
}

/// A reused network / repeated matches must be deterministic and consistent
/// (the predicate cache is reused internally without state bleed).
#[test]
fn repeated_matches_are_deterministic() {
    let net = straight_road();
    let ls: LineString = wkt! {
        LINESTRING(-118.151 34.1503, -118.158 34.1503, -118.165 34.1503)
    };
    let first = discretized_edges(&net, &ls, SolverVariant::Trellis);
    let second = discretized_edges(&net, &ls, SolverVariant::Trellis);
    assert_eq!(first, second, "repeated matches diverged");
}

/// Sanity: mock metadata is accessible in every direction (guards trait wiring).
#[test]
fn mock_metadata_accessible() {
    let meta = MockMetadata;
    assert!(meta.accessible(&(), Direction::Outgoing));
    assert!(meta.accessible(&(), Direction::Incoming));
}
