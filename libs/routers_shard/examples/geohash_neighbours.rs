//! Geohash strategy with neighbour padding, plus a quick A* route that
//! demonstrates the network is fully traversable end-to-end.

use geo::Point;
use routers_fixtures::{SYDNEY, fixture};
use routers_network::Route;
use routers_shard::{
    GeohashStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy,
    osm::OsmSource,
};

fn main() {
    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = GeohashStrategy::with_precision(5);

    let anchor = Point::new(151.2093, -33.8688);
    let owned = strategy.locate(anchor);
    let selection = Selection::new(&strategy, owned.clone(), SelectionMode::OwnedAndNeighbours);

    let started = std::time::Instant::now();
    let net = ShardedNetwork::from_source(&source, &strategy, &selection).expect("ingest");
    println!(
        "Loaded {} geohashes around {} in {}ms: {} nodes, {} edges",
        net.loaded.len(),
        owned.0,
        started.elapsed().as_millis(),
        net.num_nodes(),
        net.graph.edge_count(),
    );

    // Sanity-route from the first node in the graph to the last. Picks
    // whatever pair iteration order gives us; the goal is to prove the
    // routing machinery is wired up, not to optimise the trip.
    let mut nodes = net.hash.keys().copied();
    if let (Some(start), Some(finish)) = (nodes.next(), nodes.last()) {
        match net.route_nodes(start, finish) {
            Some((weight, path)) => {
                println!("Route: {} → {} ({} nodes, weight {weight})", start.identifier, finish.identifier, path.len())
            }
            None => println!("No route between {} and {}", start.identifier, finish.identifier),
        }
    }
}
