//! Load a single quad-tree shard from an OSM PBF.
//!
//! Picks a point in central Sydney, locates the depth-10 quad-tree cell
//! containing it, and ingests *only* the road network inside that cell.

use geo::Point;
use routers_fixtures::{SYDNEY, fixture};
use routers_shard::{
    QuadTreeStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy,
    osm::OsmSource,
};

fn main() {
    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = QuadTreeStrategy::with_depth(10);

    let anchor = Point::new(151.2093, -33.8688);
    let owned = strategy.locate(anchor);
    let bounds = strategy.bounds(&owned);

    println!("Anchor:  {anchor:?}");
    println!("Shard:   {owned:?}");
    println!(
        "Bounds:  x=[{:.4}, {:.4}], y=[{:.4}, {:.4}]",
        bounds.min().x,
        bounds.max().x,
        bounds.min().y,
        bounds.max().y,
    );

    let selection = Selection::new(&strategy, owned, SelectionMode::Owned);
    let started = std::time::Instant::now();
    let net = ShardedNetwork::from_source(&source, &strategy, &selection).expect("ingest");
    println!("Ingest:  {}ms", started.elapsed().as_millis());

    println!(
        "Network: {} nodes, {} edges, {} ways, 1 loaded shard",
        net.num_nodes(),
        net.graph.edge_count(),
        net.meta.len(),
    );
}
