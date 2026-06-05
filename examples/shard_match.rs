use geo::{Centroid, Coord, LineString, Point, centroid};
use routers::r#match::MatchSimpleExt;
use routers_fixtures::{SYDNEY, SYNDEY_TRIP, fixture};
use routers_shard::{
    QuadTreeStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy, osm::OsmSource,
};
use wkt::TryFromWkt;

fn main() {
    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(SYNDEY_TRIP).expect("must parse");
    let anchor = coordinates.centroid().expect("must have a centroid");

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

    let matched: LineString<f64> = route.discretized.iter().map(|v| Point(v.point)).collect();

    println!("Matched {} points across sharded network", matched.0.len());
}
