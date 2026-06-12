/// One-shot tool: build all geohash shard files from a Los Angeles PBF.
///
/// Usage (from libs/routers_realtime):
///   cargo run --release --example build_shards
///
/// Output: target/shard_cache/*.shard.rt
use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{SYDNEY, SYDNEY_SAVED, fixture};
use routers_network::edge::Weight;
use routers_shard::{
    Geohash, GeohashStrategy, Selection, SelectionMode, ShardSource, ShardedNetwork,
    ShardingStrategy,
};
use std::collections::HashSet;
use std::path::Path;

struct OsmSource<'a>(&'a OsmNetwork);

impl<'a> ShardSource<OsmEntryId, OsmEdgeMetadata> for OsmSource<'a> {
    fn nodes<'b>(&'b self) -> Box<dyn Iterator<Item = (OsmEntryId, Point)> + 'b> {
        Box::new(self.0.hash.values().map(|n| (n.id, n.position)))
    }

    fn edges<'b>(&'b self) -> Box<dyn Iterator<Item = (OsmEntryId, OsmEntryId, Weight, OsmEdgeMetadata)> + 'b> {
        Box::new(self.0.graph.all_edges().filter_map(|(from, to, (weight, edge_id))| {
            let meta = self.0.meta.get(&edge_id.index())?.clone();
            Some((from, to, *weight, meta))
        }))
    }
}

fn main() {
    env_logger::init();

    let out_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/shard_cache");
    std::fs::create_dir_all(&out_dir).expect("create shard_cache dir");

    eprintln!("Loading OsmNetwork from PBF...");
    let network = OsmNetwork::from_pbf_and_save(
        fixture!(SYDNEY),
        fixture!(SYDNEY_SAVED),
    ).expect("load PBF");
    eprintln!("  {} nodes, {} edges", network.hash.len(), network.graph.edge_count());

    let strategy = GeohashStrategy::with_precision(4);
    let source = OsmSource(&network);

    // Collect all unique geohash cells that contain at least one node.
    let mut cells: HashSet<Geohash> = HashSet::new();
    for node in network.hash.values() {
        cells.insert(strategy.locate(node.position));
    }
    eprintln!("  {} unique geohash cells (precision=4)", cells.len());

    let mut built = 0usize;
    let mut skipped = 0usize;
    for cell in &cells {
        let path = out_dir.join(format!("{}.shard.rt", cell));
        let selection = Selection::new(&strategy, *cell, SelectionMode::Owned);
        match ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, Geohash>::from_source(&source, &strategy, &selection) {
            Ok(net) => {
                if let Err(e) = net.save_to_file(&path) {
                    eprintln!("  WARN: failed to save {cell}: {e}");
                    skipped += 1;
                } else {
                    built += 1;
                }
            }
            Err(e) => {
                eprintln!("  WARN: from_source failed for {cell}: {e}");
                skipped += 1;
            }
        }
    }

    // Write manifest
    let manifest = out_dir.join("manifest.txt");
    let names: Vec<String> = cells.iter().map(|c| format!("{}.shard.rt", c)).collect();
    std::fs::write(&manifest, names.join("\n")).expect("write manifest");

    eprintln!("Done: {built} shards built, {skipped} skipped → {}", out_dir.display());
}
