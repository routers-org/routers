//! Demonstrates per-shard caching.
//!
//! First run rebuilds the shard from PBF and writes a `.shard.rt` file to
//! `target/shard_cache/`; subsequent runs hit the cache and skip the PBF
//! pass entirely. Reports the difference in wall-clock time.
//!
//! Spatial indices are deliberately not serialised — they are rebuilt by
//! `bulk_load` on cache hit, which is faster (and produces a smaller cache
//! file) than letting `postcard` traverse the `rstar` tree.

use std::path::PathBuf;
use std::time::Instant;

use geo::Point;
use routers_fixtures::{SYDNEY, fixture};
use routers_shard::{
    QuadTreeStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy, osm::OsmSource,
};

fn cache_path(owned: &routers_shard::QuadKey) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../target/shard_cache");
    p.push(format!("sydney_d{}_{}.shard.rt", owned.depth, owned.bits));
    p
}

fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp(None)
        .init();
    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = QuadTreeStrategy::with_depth(10);
    let owned = strategy.locate(Point::new(151.2093, -33.8688));
    let selection = Selection::new(&strategy, owned, SelectionMode::OwnedAndNeighbours);
    let path = cache_path(&owned);

    // First pass: may build from PBF or hit cache, depending on whether
    // this example has been run before.
    let warm = path.exists();
    let started = Instant::now();
    let net = ShardedNetwork::from_source_or_cache(&source, &strategy, &selection, &path)
        .expect("ingest");
    let first = started.elapsed();
    println!(
        "{:<13}{}ms  ({} loaded, {} nodes, {} edges){}",
        if warm { "cache hit:" } else { "cold build:" },
        first.as_millis(),
        net.loaded.len(),
        net.num_nodes(),
        net.graph.edge_count(),
        if warm { "" } else { "  → cache written" }
    );

    // Second pass: guaranteed cache hit.
    let started = Instant::now();
    let net = ShardedNetwork::from_source_or_cache(&source, &strategy, &selection, &path)
        .expect("ingest");
    let second = started.elapsed();
    println!(
        "{:<13}{}ms  ({} loaded, {} nodes, {} edges)",
        "cache hit:",
        second.as_millis(),
        net.loaded.len(),
        net.num_nodes(),
        net.graph.edge_count(),
    );

    if !warm {
        println!(
            "Speed-up: {:.1}x (cache vs PBF)",
            first.as_secs_f64() / second.as_secs_f64()
        );
    }
}
