//! Compare `SelectionMode::Owned` against `SelectionMode::OwnedAndNeighbours`.
//!
//! Same anchor, same strategy, same fixture — only the selection mode
//! differs. Demonstrates the "9-cell" loading pattern a node would use
//! when it needs a one-shard halo around its owned territory for handover
//! continuity.

use geo::Point;
use routers_fixtures::{SYDNEY, fixture};
use routers_shard::{
    QuadTreeStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy, osm::OsmSource,
};

fn main() {
    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = QuadTreeStrategy::with_depth(10);

    let anchor = Point::new(151.2093, -33.8688);
    let owned = strategy.locate(anchor);

    for mode in [SelectionMode::Owned, SelectionMode::OwnedAndNeighbours] {
        let selection = Selection::new(&strategy, owned, mode);
        let started = std::time::Instant::now();
        let net = ShardedNetwork::from_source(&source, &strategy, &selection).expect("ingest");
        println!(
            "{:>22?}  loaded={:>2}  nodes={:>6}  edges={:>6}  in {}ms",
            mode,
            net.loaded.len(),
            net.num_nodes(),
            net.graph.edge_count(),
            started.elapsed().as_millis(),
        );
    }
}
