//! Tests for [`ShardedNetwork`] using the in-memory synthetic source.
//!
//! These verify the *generic* builder path — no OSM dependency, no PBF.

mod common;

use common::MemSource;
use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::{Discovery, Network, Route, Scan};
use routers_shard::{
    QuadKey, QuadTreeStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy,
};

fn build(
    source: &MemSource,
    depth: u8,
    anchor: Point,
    mode: SelectionMode,
) -> ShardedNetwork<OsmEntryId, OsmEdgeMetadata, QuadKey> {
    let strategy = QuadTreeStrategy::with_depth(depth);
    let owned = strategy.locate(anchor);
    let selection = Selection::new(&strategy, owned, mode);
    ShardedNetwork::from_source(source, &strategy, &selection).expect("ingest")
}

#[test]
fn entire_grid_loads_at_shallow_depth() {
    // At depth 1 the whole grid fits in one shard, so owned-only should
    // load every node and every way.
    let source = MemSource::grid(Point::new(0.0, 0.0), 5, 5, 0.001);
    let net = build(&source, 1, Point::new(0.001, 0.001), SelectionMode::Owned);
    assert_eq!(net.num_nodes(), 25);
    // 4 horizontals/row * 5 rows + 4 verticals/col * 5 cols = 40 ways,
    // each bidirectional = 80 directed edges.
    assert_eq!(net.graph.edge_count(), 80);
}

#[test]
fn deep_shard_keeps_only_local_nodes_plus_halo() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 8, 8, 0.5);
    let net_owned = build(&source, 6, Point::new(2.0, 2.0), SelectionMode::Owned);
    let net_full = build(&source, 1, Point::new(2.0, 2.0), SelectionMode::Owned);
    assert!(net_owned.num_nodes() < net_full.num_nodes());
    assert!(net_owned.num_nodes() > 0);
}

#[test]
fn route_succeeds_within_loaded_subgraph() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 6, 6, 0.01);
    let net = build(&source, 1, Point::new(0.03, 0.03), SelectionMode::Owned);
    // Pick a start in the SW corner and a finish in the NE corner.
    let start = OsmEntryId::node(1);
    let finish = OsmEntryId::node(36);
    let route = net.route_nodes(start, finish).expect("route should exist");
    // Manhattan distance on a 6×6 grid with unit weights = 10.
    assert_eq!(route.0, 10);
    assert!(route.1.len() >= 2);
    assert_eq!(route.1.first().unwrap().id, start);
    assert_eq!(route.1.last().unwrap().id, finish);
}

#[test]
fn point_and_metadata_lookups() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 3, 3, 0.01);
    let net = build(&source, 1, Point::new(0.01, 0.01), SelectionMode::Owned);
    let id = OsmEntryId::node(1);
    let p = net.point(&id).expect("node should exist");
    assert!((p.x() - 0.0).abs() < 1e-9 && (p.y() - 0.0).abs() < 1e-9);
    // The way metadata for any retained way should be present.
    let way_id = net.meta.keys().next().copied().expect("at least one way");
    assert!(net.metadata(&way_id).is_some());
}

#[test]
fn edges_into_and_outof_are_inverses_for_bidirectional_ways() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 3, 3, 0.01);
    let net = build(&source, 1, Point::new(0.01, 0.01), SelectionMode::Owned);
    let mid = OsmEntryId::node(5); // centre of the 3×3 grid
    let out = net.edges_outof(mid).count();
    let into = net.edges_into(mid).count();
    assert_eq!(out, 4, "centre node has 4 outgoing edges in bidi grid");
    assert_eq!(into, 4, "centre node has 4 incoming edges");
}

#[test]
fn discovery_box_returns_expected_nodes() {
    use rstar::AABB;
    let source = MemSource::grid(Point::new(0.0, 0.0), 5, 5, 0.01);
    let net = build(&source, 1, Point::new(0.01, 0.01), SelectionMode::Owned);
    let box_ = AABB::from_corners(Point::new(-0.001, -0.001), Point::new(0.011, 0.011));
    // That covers the SW 2×2 — exactly 4 nodes.
    let n = net.nodes_in_box(box_).count();
    assert_eq!(n, 4);
}

#[test]
fn nearest_node_picks_closest() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 3, 3, 0.01);
    let net = build(&source, 1, Point::new(0.01, 0.01), SelectionMode::Owned);
    let target = Point::new(0.0099, 0.0099); // virtually on top of node 5
    let nearest = net.nearest_node(&target).expect("must find one");
    assert_eq!(nearest.id, OsmEntryId::node(5));
}

#[test]
fn fatten_resolves_endpoints() {
    use routers_network::{DirectionAwareEdgeId, Edge};
    let source = MemSource::grid(Point::new(0.0, 0.0), 2, 2, 0.01);
    let net = build(&source, 1, Point::new(0.01, 0.01), SelectionMode::Owned);
    let edge = Edge {
        source: OsmEntryId::node(1),
        target: OsmEntryId::node(2),
        weight: 1,
        id: DirectionAwareEdgeId::new(OsmEntryId::way(1_000_000)),
    };
    let fat = net.fatten(&edge).expect("endpoints exist");
    assert_eq!(fat.source.id, OsmEntryId::node(1));
    assert_eq!(fat.target.id, OsmEntryId::node(2));
}

#[test]
fn line_filters_unknown_ids() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 3, 3, 0.01);
    let net = build(&source, 1, Point::new(0.01, 0.01), SelectionMode::Owned);
    let ids = vec![
        OsmEntryId::node(1),
        OsmEntryId::node(999), // does not exist
        OsmEntryId::node(2),
    ];
    let line = net.line(&ids);
    assert_eq!(line.len(), 2, "unknown nodes should be silently dropped");
}

#[test]
fn neighbour_mode_increases_node_count_when_grid_spans_shards() {
    // Make a grid that *must* span multiple shards by using a coarse step
    // and a deep tree, then verify that loading neighbours grows the
    // ingested graph monotonically.
    let source = MemSource::grid(Point::new(0.0, 0.0), 16, 16, 1.0);
    let net_owned = build(&source, 4, Point::new(0.5, 0.5), SelectionMode::Owned);
    let net_padded = build(
        &source,
        4,
        Point::new(0.5, 0.5),
        SelectionMode::OwnedAndNeighbours,
    );
    assert!(net_padded.num_nodes() > net_owned.num_nodes());
    assert!(net_padded.graph.edge_count() > net_owned.graph.edge_count());
    assert!(net_padded.loaded.len() > net_owned.loaded.len());
}

#[test]
fn empty_selection_yields_nothing_useful() {
    // If the anchor is in deep ocean / outside the grid extents, the owned
    // shard collects no nodes, and we still get back a coherent (if empty)
    // network rather than a panic.
    let source = MemSource::grid(Point::new(0.0, 0.0), 3, 3, 0.001);
    let strategy = QuadTreeStrategy::with_depth(20);
    let owned = strategy.locate(Point::new(100.0, -50.0));
    let selection = Selection::new(&strategy, owned, SelectionMode::Owned);
    let net = ShardedNetwork::from_source(&source, &strategy, &selection).expect("ingest");
    assert_eq!(net.num_nodes(), 0);
    assert_eq!(net.graph.edge_count(), 0);
    assert!(
        net.route_nodes(OsmEntryId::node(1), OsmEntryId::node(2))
            .is_none()
    );
}

#[test]
fn debug_format_summarises_state() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 3, 3, 0.01);
    let net = build(&source, 1, Point::new(0.01, 0.01), SelectionMode::Owned);
    let s = format!("{net:?}");
    assert!(s.contains("ShardedNetwork"));
    assert!(s.contains("owned="));
    assert!(s.contains("nodes="));
}

#[test]
fn filter_keep_ways_where_drops_unwanted_ways() {
    use routers_shard::IngestFilter;
    // Filter drops every way (the closure always returns false).
    let source = MemSource::grid(Point::new(0.0, 0.0), 4, 4, 0.01);
    let strategy = QuadTreeStrategy::with_depth(1);
    let owned = strategy.locate(Point::new(0.01, 0.01));
    let selection = Selection::new(&strategy, owned, SelectionMode::Owned);
    let filter = IngestFilter::<OsmEdgeMetadata>::new().keep_ways_where(|_| false);
    let net = ShardedNetwork::from_source_filtered(&source, &strategy, &selection, &filter)
        .expect("ingest");
    assert_eq!(net.graph.edge_count(), 0, "filter should drop every way");
    // No edges → no nodes after the retain step.
    assert_eq!(net.num_nodes(), 0);
}

#[test]
fn filter_keep_ways_where_predicates_compose() {
    use routers_shard::IngestFilter;
    let source = MemSource::grid(Point::new(0.0, 0.0), 3, 3, 0.01);
    let strategy = QuadTreeStrategy::with_depth(1);
    let owned = strategy.locate(Point::new(0.01, 0.01));
    let selection = Selection::new(&strategy, owned, SelectionMode::Owned);
    // Both predicates must pass; the second one rejects everything.
    let filter = IngestFilter::<OsmEdgeMetadata>::new()
        .keep_ways_where(|_| true)
        .keep_ways_where(|_| false);
    let net = ShardedNetwork::from_source_filtered(&source, &strategy, &selection, &filter)
        .expect("ingest");
    assert_eq!(net.graph.edge_count(), 0);
}

#[test]
fn filter_without_metadata_drops_meta_map_but_keeps_topology() {
    use routers_network::Network;
    use routers_shard::IngestFilter;
    let source = MemSource::grid(Point::new(0.0, 0.0), 3, 3, 0.01);
    let strategy = QuadTreeStrategy::with_depth(1);
    let owned = strategy.locate(Point::new(0.01, 0.01));
    let selection = Selection::new(&strategy, owned, SelectionMode::Owned);

    let baseline =
        ShardedNetwork::from_source(&source, &strategy, &selection).expect("baseline ingest");
    let filter = IngestFilter::<OsmEdgeMetadata>::new().without_metadata();
    let stripped = ShardedNetwork::from_source_filtered(&source, &strategy, &selection, &filter)
        .expect("ingest");

    // Topology is identical.
    assert_eq!(stripped.num_nodes(), baseline.num_nodes());
    assert_eq!(stripped.graph.edge_count(), baseline.graph.edge_count());
    // But the metadata map is empty.
    assert_eq!(stripped.meta.len(), 0);
    let way_id = baseline.meta.keys().next().copied().unwrap();
    assert!(baseline.metadata(&way_id).is_some());
    assert!(stripped.metadata(&way_id).is_none());
}

#[test]
fn filter_makes_smaller_cache() {
    use routers_shard::IngestFilter;
    let source = MemSource::grid(Point::new(0.0, 0.0), 4, 4, 0.01);
    let strategy = QuadTreeStrategy::with_depth(1);
    let owned = strategy.locate(Point::new(0.01, 0.01));
    let selection = Selection::new(&strategy, owned, SelectionMode::Owned);

    let full = ShardedNetwork::from_source(&source, &strategy, &selection).expect("full");
    let stripped = ShardedNetwork::from_source_filtered(
        &source,
        &strategy,
        &selection,
        &IngestFilter::<OsmEdgeMetadata>::new().without_metadata(),
    )
    .expect("stripped");

    let mut full_path = std::env::temp_dir();
    full_path.push(format!("rs_filter_full_{}.rt", std::process::id()));
    let mut strip_path = std::env::temp_dir();
    strip_path.push(format!("rs_filter_strip_{}.rt", std::process::id()));

    full.save_to_file(&full_path).expect("save full");
    stripped.save_to_file(&strip_path).expect("save stripped");

    let full_size = std::fs::metadata(&full_path).unwrap().len();
    let strip_size = std::fs::metadata(&strip_path).unwrap().len();
    let _ = std::fs::remove_file(&full_path);
    let _ = std::fs::remove_file(&strip_path);

    assert!(
        strip_size < full_size,
        "stripped cache ({strip_size}) should be smaller than full ({full_size})"
    );
}

#[test]
fn from_cached_round_trips_via_disk() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 4, 4, 0.01);
    let net = build(&source, 1, Point::new(0.01, 0.01), SelectionMode::Owned);

    let mut path = std::env::temp_dir();
    path.push(format!("routers_shard_test_{}.rt", std::process::id()));
    net.save_to_file(&path).expect("save");

    let loaded =
        ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, QuadKey>::from_cached(&path).expect("load");
    let _ = std::fs::remove_file(&path);

    assert_eq!(loaded.num_nodes(), net.num_nodes());
    assert_eq!(loaded.graph.edge_count(), net.graph.edge_count());
    assert_eq!(loaded.owned, net.owned);
    // Indices rebuilt — confirm spatial lookups still work.
    let p = Point::new(0.01, 0.01);
    assert!(loaded.nearest_node(&p).is_some());
    assert!(
        loaded
            .nodes_in_box(rstar::AABB::from_corners(
                Point::new(-1.0, -1.0),
                Point::new(1.0, 1.0)
            ))
            .count()
            > 0
    );
}

#[test]
fn from_source_or_cache_uses_cache_on_second_call() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 4, 4, 0.01);
    let strategy = QuadTreeStrategy::with_depth(1);
    let owned = strategy.locate(Point::new(0.01, 0.01));
    let selection = Selection::new(&strategy, owned, SelectionMode::Owned);

    let mut path = std::env::temp_dir();
    path.push(format!(
        "routers_shard_test_or_cache_{}.rt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let first = ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, QuadKey>::from_source_or_cache(
        &source, &strategy, &selection, &path,
    )
    .expect("first build");
    assert!(path.exists());

    let second = ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, QuadKey>::from_source_or_cache(
        &source, &strategy, &selection, &path,
    )
    .expect("cached load");
    let _ = std::fs::remove_file(&path);

    assert_eq!(first.num_nodes(), second.num_nodes());
    assert_eq!(first.graph.edge_count(), second.graph.edge_count());
}

#[test]
fn serde_roundtrip_preserves_topology() {
    let source = MemSource::grid(Point::new(0.0, 0.0), 4, 4, 0.01);
    let net = build(&source, 1, Point::new(0.01, 0.01), SelectionMode::Owned);

    let bytes = postcard::to_allocvec(&net).expect("serialise");
    let back: ShardedNetwork<OsmEntryId, OsmEdgeMetadata, QuadKey> =
        postcard::from_bytes(&bytes).expect("deserialise");

    assert_eq!(back.num_nodes(), net.num_nodes());
    assert_eq!(back.graph.edge_count(), net.graph.edge_count());
    assert_eq!(back.owned, net.owned);
    assert_eq!(back.loaded, net.loaded);
    // The spatial indices are #[serde(skip)] — they should come back empty.
    // Re-checking topology via the graph is sufficient for the round-trip.
    let start = OsmEntryId::node(1);
    let finish = OsmEntryId::node(16);
    let r_a = net.route_nodes(start, finish).map(|r| r.0);
    let r_b = back.route_nodes(start, finish).map(|r| r.0);
    assert_eq!(r_a, r_b);
}
