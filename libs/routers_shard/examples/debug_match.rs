//! Diagnostic: load each geohash shard the build pipeline wrote and try
//! to match `SYNDEY_TRIP` against it. Reports which shard each trip
//! point falls in, whether nodes are nearby, and whether `match_simple`
//! succeeds — pinpointing whether the failure is in shard contents,
//! spatial index, or routing.

use std::path::PathBuf;

use geo::{LineString, Point};
use routers::r#match::MatchSimpleExt;
use routers::transition::costing::CostingStrategies;
use routers::transition::entity::Transition;
use routers::transition::layer::generation::StandardGenerator;
use routers::transition::solver::selective_forward::SelectiveForwardSolver;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_fixtures::SYNDEY_TRIP;
use routers_network::{DataPlane, Metadata, Scan};
use routers_shard::{Geohash, GeohashStrategy, ShardedNetwork, ShardingStrategy};
use wkt::TryFromWkt as _;

const SHARD_PRECISION: u8 = 5;

fn cache_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/shard_cache")
}

fn shard_filename(key: &Geohash) -> String {
    format!("{}.shard.rt", key.0)
}

fn main() {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp(None)
        .init();

    let line: LineString<f64> = LineString::try_from_wkt_str(SYNDEY_TRIP).expect("parse WKT");
    println!("Trip has {} points", line.0.len());

    let strategy = GeohashStrategy::with_precision(SHARD_PRECISION);

    // 1. Which shards do the trip points fall in?
    let cells: Vec<Geohash> = line
        .0
        .iter()
        .map(|c| strategy.locate(Point::new(c.x, c.y)))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    for (i, (c, g)) in line.0.iter().zip(cells.iter()).enumerate() {
        if seen.insert(g.clone()) {
            println!("  point {i}: ({:.5}, {:.5}) → {}", c.x, c.y, g.0);
        }
    }
    println!("Trip spans {} distinct cells", seen.len());

    // 2. For each unique cell, load the shard and report its state.
    for cell in &seen {
        let path = cache_dir().join(shard_filename(cell));
        println!("\n=== {} ===", cell.0);
        if !path.exists() {
            println!("  shard file not present: {}", path.display());
            continue;
        }
        let net = match ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, Geohash>::from_cached(&path)
        {
            Ok(n) => n,
            Err(e) => {
                println!("  load failed: {e}");
                continue;
            }
        };
        println!(
            "  loaded: {} nodes / {} edges / {} ways with metadata",
            net.num_nodes(),
            net.graph.edge_count(),
            net.meta.len(),
        );

        // Spatial sanity check: how close is the nearest node to each
        // trip point that falls in this cell?
        for (i, c) in line.0.iter().enumerate() {
            if cells[i] != *cell {
                continue;
            }
            let p = Point::new(c.x, c.y);
            match net.nearest_node(&p) {
                Some(n) => {
                    let dx = (n.position.x() - p.x()) * 95_000.0; // ~m at -33° lat
                    let dy = (n.position.y() - p.y()) * 111_000.0;
                    let dist_m = (dx * dx + dy * dy).sqrt();
                    println!(
                        "  point {i} ({:.5}, {:.5}) → nearest node {} at {:.0} m",
                        c.x,
                        c.y,
                        n.id.identifier,
                        dist_m
                    );
                }
                None => println!("  point {i} → no nearest node!"),
            }
        }

        // Spot-check a metadata lookup so we can see if `meta` is
        // actually wired up after load.
        if let Some((id, _meta)) = net.meta.iter().next() {
            println!(
                "  sample metadata present for way id {} (DataPlane::metadata returns {})",
                id.identifier,
                net.metadata(id).is_some()
            );
        } else {
            println!("  WARNING: shard has 0 metadata entries");
        }

        // 3a. Try matching via `match_simple` (uses PrecomputeForwardSolver).
        println!("  trying match_simple…");
        match net.r#match_simple(line.clone()) {
            Ok(route) => println!(
                "  ✔ match_simple: {} discrete, {} interpolated",
                route.discretized.len(),
                route.interpolated.len()
            ),
            Err(e) => println!("  ✘ match_simple failed: {:?}", e),
        }

        // 3b. Replicate the *viewer*'s exact path: `Arc<ShardedNetwork>`
        // (via the routers_network Arc blanket impls), explicit
        // Transition construction, SelectiveForwardSolver. If this
        // fails while `match_simple` succeeds, the bug is wasm-side
        // or in the Arc forwarders.
        println!("  trying viewer flow (Arc<…> + SelectiveForwardSolver)…");
        let arc_net = std::sync::Arc::new(net);
        let costing = CostingStrategies::default();
        let generator = StandardGenerator::new(&arc_net, &costing.emission, 100.0);
        let transition = Transition::new(&arc_net, line.clone(), &costing, generator);
        let solver = SelectiveForwardSolver::default();
        let runtime = <OsmEdgeMetadata as Metadata>::default_runtime();
        match transition.solve(solver, &runtime) {
            Ok(collapsed) => println!(
                "  ✔ viewer flow: cost={}, route len={}",
                collapsed.cost,
                collapsed.route.len()
            ),
            Err(e) => println!("  ✘ viewer flow failed: {:?}", e),
        }
    }
}
