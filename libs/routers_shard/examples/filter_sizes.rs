//! Filter permutations vs. cache size.
//!
//! Builds the same Sydney quad-tree shard four ways:
//!
//! 1. **Full**       — every traversable way, full metadata
//! 2. **No metadata** — every way, but `OsmEdgeMetadata` is dropped
//! 3. **Tertiary+**   — only ways with `RoadClass::Tertiary` or denser,
//!                     full metadata
//! 4. **Tertiary+ stripped** — combination of (2) and (3)
//!
//! Prints the on-disk size of each variant so you can see what's
//! affordable for a WASM bundle.

use std::path::PathBuf;

use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, primitives::RoadClass};
use routers_fixtures::{SYDNEY, fixture};
use routers_shard::{
    IngestFilter, QuadKey, QuadTreeStrategy, Selection, SelectionMode, ShardedNetwork,
    ShardingStrategy, osm::OsmSource,
};

fn cache_path(name: &str, owned: &QuadKey) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../target/shard_cache");
    p.push(format!(
        "sydney_{name}_d{}_{}.shard.rt",
        owned.depth, owned.bits
    ));
    p
}

fn build(
    name: &str,
    source: &OsmSource,
    strategy: &QuadTreeStrategy,
    selection: &Selection<QuadKey>,
    filter: &IngestFilter<OsmEdgeMetadata>,
    owned: &QuadKey,
) -> (usize, usize, u64) {
    let path = cache_path(name, owned);
    let _ = std::fs::remove_file(&path);
    let net =
        ShardedNetwork::from_source_or_cache_filtered(source, strategy, selection, filter, &path)
            .expect("ingest");
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    (net.num_nodes(), net.graph.edge_count(), bytes)
}

fn main() {
    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = QuadTreeStrategy::with_depth(10);
    let owned = strategy.locate(Point::new(151.2093, -33.8688));
    let selection = Selection::new(&strategy, owned, SelectionMode::Owned);

    let full = IngestFilter::<OsmEdgeMetadata>::new();
    let no_meta = IngestFilter::<OsmEdgeMetadata>::new().without_metadata();
    let trunk = IngestFilter::<OsmEdgeMetadata>::new()
        .keep_ways_where(|m| matches!(m.road_class, Some(c) if (c as i32) <= RoadClass::Tertiary as i32));
    let trunk_no_meta = IngestFilter::<OsmEdgeMetadata>::new()
        .without_metadata()
        .keep_ways_where(|m| matches!(m.road_class, Some(c) if (c as i32) <= RoadClass::Tertiary as i32));

    let variants: [(&str, &IngestFilter<OsmEdgeMetadata>); 4] = [
        ("full", &full),
        ("no-meta", &no_meta),
        ("tertiary+", &trunk),
        ("tertiary+_no-meta", &trunk_no_meta),
    ];

    println!(
        "{:<20}{:>10}{:>10}{:>14}",
        "variant", "nodes", "edges", "cache (KB)"
    );
    let mut full_size = 0u64;
    for (name, filter) in variants {
        let (nodes, edges, bytes) = build(name, &source, &strategy, &selection, filter, &owned);
        if name == "full" {
            full_size = bytes;
        }
        let pct = if full_size > 0 {
            format!(" ({:>5.1}% of full)", 100.0 * bytes as f64 / full_size as f64)
        } else {
            String::new()
        };
        println!(
            "{:<20}{:>10}{:>10}{:>14.1}{pct}",
            name,
            nodes,
            edges,
            bytes as f64 / 1024.0
        );
    }
}
