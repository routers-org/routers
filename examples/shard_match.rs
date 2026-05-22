//! Integration example: map-match a trip against a *sharded* OSM network.
//!
//! This shows the routers_shard crate cooperating with the root `routers`
//! crate. The network is built from a single quad-tree shard *plus its
//! neighbours* so that the matched trip — which strays outside any single
//! cell — still has continuous coverage end-to-end.
//!
//! Run with:
//!   cargo run --example shard_match --features shard

use geo::{Coord, LineString, Point};
use routers::r#match::MatchSimpleExt;
use routers_fixtures::{SYDNEY, SYNDEY_TRIP, fixture};
use routers_shard::{
    QuadTreeStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy,
    osm::OsmSource,
};
use wkt::TryFromWkt;

fn centroid(line: &LineString<f64>) -> Point {
    let (sx, sy, n) = line.0.iter().fold((0.0, 0.0, 0u32), |(sx, sy, n), Coord { x, y }| {
        (sx + x, sy + y, n + 1)
    });
    Point::new(sx / n as f64, sy / n as f64)
}

fn main() {
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(SYNDEY_TRIP).expect("must parse");
    let anchor = centroid(&coordinates);

    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = QuadTreeStrategy::with_depth(9);
    let owned = strategy.locate(anchor);
    let selection = Selection::new(&strategy, owned, SelectionMode::OwnedAndNeighbours);

    let started = std::time::Instant::now();
    let network = ShardedNetwork::from_source(&source, &strategy, &selection).expect("ingest");
    println!(
        "Sharded ingest: {} loaded shards, {} nodes, {} edges in {}ms",
        network.loaded.len(),
        network.num_nodes(),
        network.graph.edge_count(),
        started.elapsed().as_millis(),
    );

    let route = network
        .r#match_simple(coordinates)
        .expect("match must complete");

    let matched: LineString<f64> = route
        .discretized
        .iter()
        .map(|v| Point(v.point))
        .collect();

    println!("Matched {} points across sharded network", matched.0.len());
}
