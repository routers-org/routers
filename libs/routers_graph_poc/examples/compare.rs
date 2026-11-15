//! The compact graph against `OsmNetwork`: ingest, load, footprint, and the
//! matcher's bounded search — with the searches checked to agree exactly.
//!
//! ```sh
//! cargo run --release -p routers_graph_poc --example compare                      # system allocator
//! cargo run --release -p routers_graph_poc --example compare --features mimalloc
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use geo::LineString;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{LAX_LYNWOOD_TRIP, LOS_ANGELES, SYDNEY, SYNDEY_TRIP, VENTURA_TRIP, fixture};
use routers_graph_poc::{CompactGraph, NO_NODE, Search};
use routers_network::{Entry, Metadata, Route};
use routers_transition::costing::CostingStrategies;
use routers_transition::layer::generation::{LayerGeneration, StandardGenerator};
use routers_transition::primitives::{PredicateCache, RoutingContext};
use routers_transition::uom::si::f64::Length;
use routers_transition::uom::si::length::meter;
use routers_transition::weigh::AllCompute;
use routers_transition::{Matcher, Origin};
use rustc_hash::{FxHashMap, FxHashSet};
use wkt::TryFromWkt;

/// Resident set size, in MiB.
fn resident() -> f64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| line.split_whitespace().nth(1)?.parse::<f64>().ok())
        .map_or(0.0, |kib| kib / 1024.0)
}

fn timed<T>(run: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = run();
    (value, started.elapsed())
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

fn main() {
    let allocator = if cfg!(feature = "mimalloc") {
        "mimalloc"
    } else {
        "system"
    };
    println!("allocator: {allocator}\n");

    // `POC_REGION=sydney` for the small extract; Los Angeles by default.
    let sydney = std::env::var("POC_REGION").is_ok_and(|region| region == "sydney");
    let (region, pbf, trips) = if sydney {
        ("Sydney", fixture!(SYDNEY), vec![("SYDNEY", SYNDEY_TRIP)])
    } else {
        (
            "Los Angeles",
            fixture!(LOS_ANGELES),
            vec![
                ("VENTURA_HWY", VENTURA_TRIP),
                ("LAX_LYNWOOD", LAX_LYNWOOD_TRIP),
            ],
        )
    };

    // --- Ingest ----------------------------------------------------------
    let before = resident();
    let (compact, ingest_compact) = timed(|| CompactGraph::from_pbf(pbf).expect("pbf ingests"));
    let (edge_index, index_compact) = timed(|| compact.index_edges());
    let compact_resident = resident() - before;

    let before = resident();
    let (network, ingest_network) = timed(|| OsmNetwork::from_pbf(pbf).expect("pbf ingests"));
    let network_resident = resident() - before;

    println!(
        "ingest ({region}, {} nodes, {} edges)",
        compact.nodes(),
        compact.edges()
    );
    println!(
        "  OsmNetwork::from_pbf        {:>8.0} ms   (graph + node and edge r-trees)",
        ms(ingest_network)
    );
    println!(
        "  CompactGraph::from_pbf      {:>8.0} ms   + edge r-tree {:.0} ms = {:.0} ms",
        ms(ingest_compact),
        ms(index_compact),
        ms(ingest_compact + index_compact)
    );
    println!(
        "  resident growth: OsmNetwork {network_resident:.0} MiB, CompactGraph + r-tree {compact_resident:.0} MiB"
    );
    println!(
        "  CompactGraph columns: {:.0} MiB",
        compact.footprint() as f64 / 1048576.0
    );
    drop(edge_index);

    let (converted, convert) = timed(|| CompactGraph::from_network(&network));
    println!(
        "  CompactGraph::from_network  {:>8.0} ms   same layout as from_pbf: {}",
        ms(convert),
        converted.same_layout(&compact)
    );
    drop(converted);

    // --- Save / load ------------------------------------------------------
    let (bytes, save_compact) = timed(|| compact.to_bytes());
    let (loaded, load_compact) = timed(|| CompactGraph::from_bytes(&bytes).expect("loads"));
    println!("\nsave / load");
    println!(
        "  CompactGraph  {:>6.0} MiB  save {:>6.0} ms  load {:>6.0} ms   round-trips: {}",
        bytes.len() as f64 / 1048576.0,
        ms(save_compact),
        ms(load_compact),
        loaded.same_layout(&compact)
    );
    drop((bytes, loaded));

    let (bytes, save_network) = timed(|| network.to_bytes().expect("saves"));
    let (reloaded, load_network) = timed(|| OsmNetwork::from_bytes(&bytes).expect("loads"));
    println!(
        "  OsmNetwork    {:>6.0} MiB  save {:>6.0} ms  load {:>6.0} ms   (load rebuilds both r-trees)",
        bytes.len() as f64 / 1048576.0,
        ms(save_network),
        ms(load_network)
    );
    drop((bytes, reloaded));

    // --- Search -----------------------------------------------------------
    let runtime = OsmEdgeMetadata::default_runtime();
    let (passable, accessibility) = timed(|| compact.accessibility(&runtime));
    let lookup = compact.lookup();
    println!(
        "\nsearch (single thread, cold caches; accessibility mask built once in {:.0} ms)",
        ms(accessibility)
    );

    let costing = CostingStrategies::default();
    for (name, trip) in trips {
        let line = LineString::<f64>::try_from_wkt_str(trip).expect("trip parses");
        let points = line.into_points();

        // The roots a match searches from: each candidate's edge target, in
        // trajectory order, once.
        let generator = StandardGenerator::new(&network, &costing.emission);
        let layers = generator.generate_all(&points);
        let mut known = FxHashSet::default();
        let roots: Vec<OsmEntryId> = layers
            .iter()
            .flatten()
            .map(|candidate| candidate.edge.target)
            .filter(|root| known.insert(*root))
            .collect();

        // A trip, only for the routing context the existing search needs.
        let matcher = Matcher::new(
            &network,
            &costing,
            generator,
            AllCompute::default(),
            &runtime,
        );
        let mut trip = matcher.begin();
        let origins = points
            .iter()
            .enumerate()
            .map(|(index, &point)| Origin::new(point, index as i64))
            .collect::<Vec<_>>();
        matcher.extend(&mut trip, &origins).expect("trip anchors");
        let ctx = RoutingContext {
            candidates: trip.candidates(),
            map: &network,
            runtime: &runtime,
        };

        // --- Point-to-point routing ----------------------------------------
        // Between road nodes a few dozen candidates apart along the trace.
        let pairs = roots
            .iter()
            .zip(roots.iter().skip(40))
            .step_by(7)
            .map(|(&from, &to)| (from, to))
            .collect::<Vec<_>>();

        let (existing_routes, spent_existing) = timed(|| {
            pairs
                .iter()
                .map(|&(from, to)| network.route_nodes(from, to).map(|(cost, _)| cost))
                .collect::<Vec<_>>()
        });

        let mut router = Search::new(&compact);
        let (compact_routes, spent_compact) = timed(|| {
            pairs
                .iter()
                .map(|&(from, to)| {
                    let (from, to) = (lookup.node(from)?, lookup.node(to)?);
                    router.route(&compact, from, to).map(|(cost, _)| cost)
                })
                .collect::<Vec<_>>()
        });

        assert_eq!(compact_routes, existing_routes, "route costs disagree");
        let per = |spent: Duration| spent.as_secs_f64() * 1e6 / pairs.len() as f64;
        println!(
            "  {name}: {} routes ({} found)   existing {:>9.1} µs/route   compact {:>8.1} µs/route   {:>5.1}x   same costs: yes",
            pairs.len(),
            existing_routes.iter().flatten().count(),
            per(spent_existing),
            per(spent_compact),
            spent_existing.as_secs_f64() / spent_compact.as_secs_f64()
        );

        println!("  {name}: {} roots", roots.len());
        for reach in [1_000.0, 4_000.0] {
            let reach_distance = Length::new::<meter>(reach);

            // Existing: a fresh cache, so every root is searched.
            let cache = Arc::new(PredicateCache::<OsmNetwork>::with_reach_distance(
                reach_distance,
            ));
            let (existing, spent_existing) = timed(|| {
                roots
                    .iter()
                    .map(|&root| cache.query(&ctx, root))
                    .collect::<Vec<_>>()
            });

            // Compact: one workspace, reused across roots.
            let mut search = Search::new(&compact);
            let nodes = roots
                .iter()
                .map(|&root| lookup.node(root).expect("root is a node"))
                .collect::<Vec<_>>();
            let (settled, spent_compact) = timed(|| {
                nodes
                    .iter()
                    .map(|&root| {
                        search
                            .reach(&compact, &passable, root, reach_distance)
                            .to_vec()
                    })
                    .collect::<Vec<_>>()
            });

            // The two must agree exactly: same nodes, same parents.
            let mut total = 0;
            for ((&root, existing), settled) in roots.iter().zip(&existing).zip(&settled) {
                let compact_parents = settled
                    .iter()
                    .map(|&(node, parent)| {
                        let parent = if parent == NO_NODE {
                            // The existing search gives the root the null id.
                            OsmEntryId::default().identifier()
                        } else {
                            compact.id(parent)
                        };
                        (compact.id(node), parent)
                    })
                    .collect::<FxHashMap<_, _>>();
                let existing_parents = existing
                    .iter()
                    .map(|(node, parent)| (node.identifier(), parent.identifier()))
                    .collect::<FxHashMap<_, _>>();
                assert_eq!(
                    compact_parents,
                    existing_parents,
                    "search from {} disagrees",
                    root.identifier()
                );
                total += settled.len();
            }

            let per = |spent: Duration| spent.as_secs_f64() * 1e6 / roots.len() as f64;
            println!(
                "    reach {:>4.0} m: {:>6.0} nodes/search   existing {:>8.1} µs/search   compact {:>7.2} µs/search   {:>5.1}x   identical: yes",
                reach,
                total as f64 / roots.len() as f64,
                per(spent_existing),
                per(spent_compact),
                spent_existing.as_secs_f64() / spent_compact.as_secs_f64()
            );
        }
    }
}
