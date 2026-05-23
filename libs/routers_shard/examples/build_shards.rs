//! Server-side build pipeline: walks a bounding box at a fixed quad-tree
//! depth and writes one `.shard.rt` per non-empty cell.
//!
//! The output directory can then be served as static files; the browser's
//! `WebShardFetcher` reads from the same URL prefix. The naming convention
//! used here (`d{depth}_{bits}.shard.rt`) matches the `naming` closure
//! example in the docs — keep them in sync if you change one.

use std::path::PathBuf;
use std::time::Instant;

use geo::Point;
use routers_codec::osm::OsmEdgeMetadata;
use routers_codec::osm::primitives::RoadClass;
use routers_fixtures::{SYDNEY, fixture};
use routers_shard::{
    IngestFilter, QuadKey, QuadTreeStrategy, Selection, SelectionMode, ShardedNetwork,
    ShardingStrategy, osm::OsmSource,
};

fn naming(key: &QuadKey) -> String {
    format!("d{}_{}.shard.rt", key.depth, key.bits)
}

fn iter_grid(strategy: &QuadTreeStrategy, sw: Point, ne: Point) -> Vec<QuadKey> {
    // Walk a regular grid of sample points at the cell pitch implied by the
    // strategy's depth. Dedupe along the way so a single cell only appears
    // once even when multiple samples land in it.
    let root_w = 360.0_f64;
    let root_h = 180.0_f64;
    let cells = 1u64 << strategy.depth();
    let cell_w = root_w / cells as f64;
    let cell_h = root_h / cells as f64;

    let mut out = Vec::new();
    let mut seen = rustc_hash::FxHashSet::default();
    let mut y = sw.y();
    while y <= ne.y() {
        let mut x = sw.x();
        while x <= ne.x() {
            let k = strategy.locate(Point::new(x, y));
            if seen.insert(k) {
                out.push(k);
            }
            x += cell_w * 0.5;
        }
        y += cell_h * 0.5;
    }
    out
}

fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp(None)
        .init();

    // Configurable knobs. In a real pipeline these would come from
    // CLI args or a config file.
    let depth: u8 = 10;
    let sw = Point::new(151.10, -33.95);
    let ne = Point::new(151.30, -33.80);
    let out_dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/shard_cache");
    // Conservative filter — surface routes only, no per-way metadata —
    // matches what a WASM bundle would actually want to fetch.
    let filter = IngestFilter::<OsmEdgeMetadata>::new()
        .without_metadata()
        .keep_ways_where(
            |m| matches!(m.road_class, Some(c) if (c as i32) <= RoadClass::Tertiary as i32),
        );

    std::fs::create_dir_all(&out_dir).expect("mkdir");

    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = QuadTreeStrategy::with_depth(depth);

    let shards = iter_grid(&strategy, sw, ne);
    println!(
        "Building {} shards at depth {} into {}",
        shards.len(),
        depth,
        out_dir.display(),
    );

    let started = Instant::now();
    let mut written = 0usize;
    let mut empty = 0usize;
    let mut total_bytes: u64 = 0;
    for key in &shards {
        let selection = Selection::new(&strategy, *key, SelectionMode::Owned);
        let net = ShardedNetwork::from_source_filtered(&source, &strategy, &selection, &filter)
            .expect("build");
        if net.num_nodes() == 0 {
            empty += 1;
            continue;
        }
        let path = out_dir.join(naming(key));
        net.save_to_file(&path).expect("write");
        let sz = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        total_bytes += sz;
        written += 1;
        println!(
            "  {:?}: {:>6} nodes / {:>6} edges / {:>6.1} KB",
            key,
            net.num_nodes(),
            net.graph.edge_count(),
            sz as f64 / 1024.0,
        );
    }

    println!(
        "Done. {} shards written, {} empty, {:.2} MB total, {:?} elapsed.",
        written,
        empty,
        total_bytes as f64 / 1_048_576.0,
        started.elapsed(),
    );

    // Drop a small manifest so the browser can discover what's available
    // without probing every possible cell. JSON keeps it readable; the
    // browser loader can fetch + parse this once at startup.
    let manifest_path = out_dir.join("manifest.json");
    let entries: Vec<String> = shards
        .iter()
        .filter_map(|k| {
            let p = out_dir.join(naming(k));
            p.exists().then(|| {
                format!(
                    "{{\"depth\":{},\"bits\":{},\"file\":\"{}\"}}",
                    k.depth,
                    k.bits,
                    naming(k)
                )
            })
        })
        .collect();
    let manifest = format!("{{\"shards\":[{}]}}", entries.join(","));
    std::fs::write(&manifest_path, manifest).expect("write manifest");
    println!("Wrote manifest to {}", manifest_path.display());
}
