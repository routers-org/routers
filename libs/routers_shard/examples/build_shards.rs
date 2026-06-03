//! Server-side build pipeline: walks a bounding box and writes one
//! `.shard.rt` per non-empty geohash cell.
//!
//! The output directory can then be served as static files; the browser's
//! `WebShardFetcher` reads from the same URL prefix. The naming convention
//! used here (`<geohash>.shard.rt`, e.g. `r3gx2.shard.rt`) matches the
//! parser in `libs/routers_viewer/src/bin/web_viewer.rs` — keep them in
//! sync if you change one.
//!
//! Geohash precision picks the cell size:
//!
//! - 4 → ~39 km × 19 km (too coarse for a city)
//! - **5 → ~4.9 km × 4.9 km** (a CBD-sized cell — current default)
//! - 6 → ~1.2 km × 600 m (too fine — fragmented road network)

use std::path::{Path, PathBuf};
use std::time::Instant;

use geo::Point;
use routers_codec::osm::primitives::RoadClass;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_fixtures::{SYDNEY, fixture};
use routers_shard::{
    Geohash, GeohashStrategy, IngestFilter, Selection, SelectionMode, ShardedNetwork,
    ShardingStrategy, osm::OsmSource,
};

fn naming(key: &Geohash) -> String {
    format!("{}.shard.rt", key.0)
}

/// Decide whether the existing cache at `out_dir` is still valid against
/// this binary's compiled `FORMAT_HASH`. Approach: read the first entry
/// from `manifest.txt` and `from_cached` it — that exercises the magic-
/// header + hash check that the wasm side would also do, so a "yes" from
/// here means the wasm bundle will load the cache cleanly.
///
/// Returns `Ok(())` if the cache is fresh, `Err(reason)` otherwise. The
/// reason is logged for the user; we don't propagate it because the
/// resolution is always the same — rebuild.
fn cache_is_fresh(out_dir: &Path) -> Result<(), String> {
    let manifest_path = out_dir.join("manifest.txt");
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("manifest at {}: {e}", manifest_path.display()))?;
    let first = manifest
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| "manifest is empty".to_string())?;
    let path = out_dir.join(first);
    // Just decode — we throw the result away. If it succeeds the magic
    // header and the format hash both checked out, which is exactly
    // what we want to verify.
    ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, Geohash>::from_cached(&path)
        .map(|_| ())
        .map_err(|e| format!("cache load `{first}`: {e}"))
}

/// Walk a regular grid of sample points over `[sw, ne]` and dedupe by
/// the strategy's locate function. The step size doesn't have to match
/// the cell pitch — finer is just wasted CPU at dedupe time, coarser
/// risks missing cells.
fn iter_grid<St: ShardingStrategy>(strategy: &St, sw: Point, ne: Point, step: f64) -> Vec<St::Id> {
    let mut out = Vec::new();
    let mut seen: rustc_hash::FxHashSet<St::Id> = rustc_hash::FxHashSet::default();
    let mut y = sw.y();
    while y <= ne.y() {
        let mut x = sw.x();
        while x <= ne.x() {
            let k = strategy.locate(Point::new(x, y));
            if seen.insert(k.clone()) {
                out.push(k);
            }
            x += step;
        }
        y += step;
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
    let precision: u8 = 5;
    let sw = Point::new(151.10, -33.95);
    let ne = Point::new(151.30, -33.80);
    // 0.01° ≈ 1.1 km — fine enough to never miss a precision-5 cell
    // (~4.9 km across), coarse enough that the walk is cheap.
    let sample_step: f64 = 0.01;
    let out_dir: PathBuf =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/shard_cache");
    // Keep the full metadata — the matcher uses `Metadata::accessible`
    // via `Network::metadata(id)` during candidate transitions, and
    // stripping it (`without_metadata()`) causes match failures because
    // every way looks inaccessible. Also keep every road class: trips
    // happen on residential streets too, not just trunk roads.
    //
    // Reintroduce filtering later, but only after the matcher's metadata
    // dependency is wired up to a sensible default for missing entries.
    let filter = IngestFilter::<OsmEdgeMetadata>::new();
    let _ = RoadClass::Tertiary; // keep the import warning-free while filter is empty

    std::fs::create_dir_all(&out_dir).expect("mkdir");

    // Skip the whole build if the cache on disk already matches this
    // binary's compiled `FORMAT_HASH`. Cheap to verify (one decode of
    // a single shard), and means `just web serve` can call us
    // unconditionally without paying the ~2 s PBF parse on every
    // invocation.
    match cache_is_fresh(&out_dir) {
        Ok(()) => {
            println!(
                "Shard cache at {} is fresh — skipping rebuild.",
                out_dir.display()
            );
            return;
        }
        Err(reason) => {
            println!("Cache stale or missing ({reason}) — rebuilding.");
        }
    }

    let source = OsmSource::new(fixture!(SYDNEY).clone());
    let strategy = GeohashStrategy::with_precision(precision);

    let shards = iter_grid(&strategy, sw, ne, sample_step);
    println!(
        "Building {} shards at geohash precision {} into {}",
        shards.len(),
        precision,
        out_dir.display(),
    );

    let started = Instant::now();
    let mut written = 0usize;
    let mut empty = 0usize;
    let mut total_bytes: u64 = 0;
    for key in &shards {
        let selection = Selection::new(&strategy, key.clone(), SelectionMode::Owned);
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
            "  {}: {:>6} nodes / {:>6} edges / {:>6.1} KB",
            key.0,
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

    // Drop a plain-text manifest so the browser can discover what's
    // available without probing every possible cell. One filename per
    // line keeps it trivial to parse on the wasm side without dragging
    // in `serde_json`.
    //
    // Sort by file size (ascending) so the densest shard ends up last —
    // the wasm bootstrap's `pick_starter` takes `.last()` and we want
    // first paint to land on a populated area rather than an empty
    // corner cell.
    let manifest_path = out_dir.join("manifest.txt");
    let mut entries: Vec<(u64, String)> = shards
        .iter()
        .filter_map(|k| {
            let p = out_dir.join(naming(k));
            let size = std::fs::metadata(&p).ok()?.len();
            Some((size, naming(k)))
        })
        .collect();
    entries.sort_by_key(|(sz, _)| *sz);
    let lines: Vec<String> = entries.into_iter().map(|(_, name)| name).collect();
    std::fs::write(&manifest_path, lines.join("\n") + "\n").expect("write manifest");
    println!("Wrote manifest to {}", manifest_path.display());
}
