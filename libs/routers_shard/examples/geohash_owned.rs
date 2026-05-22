//! Same flow as `quadtree_owned`, but uses [`GeohashStrategy`] instead.
//!
//! The point of this example is to show that the [`ShardSource`],
//! [`Selection`] and [`ShardedNetwork`] code paths don't care which
//! partitioning scheme is in use — only the type of the shard ID changes.

use geo::Point;
use routers_fixtures::{SYDNEY, fixture};
use routers_shard::{
    GeohashStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy,
    osm::OsmSource,
};

fn main() {
    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = GeohashStrategy::with_precision(5);

    let anchor = Point::new(151.2093, -33.8688);
    let owned = strategy.locate(anchor);
    let bounds = strategy.bounds(&owned);

    println!("Anchor:    {anchor:?}");
    println!("Geohash:   {}", owned.0);
    println!(
        "Bounds:    x=[{:.4}, {:.4}], y=[{:.4}, {:.4}]",
        bounds.min().x,
        bounds.max().x,
        bounds.min().y,
        bounds.max().y,
    );

    let selection = Selection::new(&strategy, owned.clone(), SelectionMode::Owned);
    let started = std::time::Instant::now();
    let net = ShardedNetwork::from_source(&source, &strategy, &selection).expect("ingest");
    println!("Ingest:    {}ms", started.elapsed().as_millis());

    println!(
        "Loaded geohash {}: {} nodes, {} edges",
        owned.0,
        net.num_nodes(),
        net.graph.edge_count(),
    );
}
