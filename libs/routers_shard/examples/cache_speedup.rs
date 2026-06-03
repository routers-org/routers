//! Builds 9 individual single-shard networks (one per cell in the 3×3
//! neighbourhood around the Sydney CBD), then re-loads them from cache.
//!
//! Mirrors the realistic distributed scenario where each shard is built on
//! a separate worker rather than as one bundled selection. Prints
//! per-shard timing for both passes.

use std::path::PathBuf;
use std::time::Instant;

use geo::Point;
use routers_fixtures::{SYDNEY, fixture};
use routers_shard::{
    Geohash, GeohashStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy,
    osm::OsmSource,
};

fn cache_path(owned: &Geohash) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../target/shard_cache");
    p.push(format!("sydney_geohash_{}.shard.rt", owned.0));
    p
}

fn main() {
    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = GeohashStrategy::with_precision(5);
    let centre = strategy.locate(Point::new(151.2093, -33.8688));

    // The cell we own plus its 8 cardinal neighbours — what a real worker
    // would actually iterate, building each as its own standalone network.
    let mut shards = vec![centre.clone()];
    shards.extend(strategy.neighbours(&centre));

    // Pre-clean: ensure we time genuine cold builds.
    for s in &shards {
        let _ = std::fs::remove_file(cache_path(s));
    }

    let cold_total = Instant::now();
    let mut cold = Vec::with_capacity(shards.len());
    for shard in &shards {
        let selection = Selection::new(&strategy, shard.clone(), SelectionMode::Owned);
        let started = Instant::now();
        let _ = ShardedNetwork::from_source_or_cache(
            &source,
            &strategy,
            &selection,
            &cache_path(shard),
        )
        .expect("ingest");
        cold.push(started.elapsed().as_millis());
    }
    let cold_wall = cold_total.elapsed().as_millis();

    let warm_total = Instant::now();
    let mut warm = Vec::with_capacity(shards.len());
    for shard in &shards {
        let selection = Selection::new(&strategy, shard.clone(), SelectionMode::Owned);
        let started = Instant::now();
        let _ = ShardedNetwork::from_source_or_cache(
            &source,
            &strategy,
            &selection,
            &cache_path(shard),
        )
        .expect("ingest");
        warm.push(started.elapsed().as_millis());
    }
    let warm_wall = warm_total.elapsed().as_millis();

    println!("Per-shard timing (ms):");
    println!("  cold  {:?}", cold);
    println!("  warm  {:?}", warm);
    println!(
        "Totals: cold={}ms, warm={}ms ({:.1}x faster from cache)",
        cold_wall,
        warm_wall,
        cold_wall as f64 / warm_wall.max(1) as f64,
    );

    // Per-shard cache file sizes — answers "how big is one shard vs the
    // whole-Sydney cache?".
    let sizes: Vec<u64> = shards
        .iter()
        .map(|s| {
            std::fs::metadata(cache_path(s))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .collect();
    let total: u64 = sizes.iter().sum();
    let nonempty: Vec<&u64> = sizes.iter().filter(|&&b| b > 100).collect();
    println!();
    println!("Per-shard cache sizes (bytes):");
    for (shard, bytes) in shards.iter().zip(&sizes) {
        println!(
            "  {:?}: {:>10}  ({:>6.2} MB)",
            shard,
            bytes,
            *bytes as f64 / 1_048_576.0
        );
    }
    println!("Total across all 9: {:.2} MB", total as f64 / 1_048_576.0);
    if !nonempty.is_empty() {
        let sum_nz: u64 = nonempty.iter().copied().sum();
        let mean = sum_nz as f64 / nonempty.len() as f64 / 1_048_576.0;
        let max = nonempty.iter().copied().max().unwrap_or(&0);
        let min = nonempty.iter().copied().min().unwrap_or(&0);
        println!(
            "Non-empty shards: {} cells, mean {:.2} MB, min {:.2} MB, max {:.2} MB",
            nonempty.len(),
            mean,
            *min as f64 / 1_048_576.0,
            *max as f64 / 1_048_576.0,
        );
    }
}
