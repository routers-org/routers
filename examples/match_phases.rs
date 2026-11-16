//! Times each phase of a match apart.
//!
//! ```sh
//! cargo run --release --example match_phases           # warm cache
//! cargo run --release --example match_phases -- cold   # a fresh cache per match
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use geo::LineString;
use routers::candidate::RoutedPath;
use routers::costing::CostingStrategies;
use routers::layer::generation::StandardGenerator;
use routers::primitives::PredicateCache;
use routers::weigh::AllCompute;
use routers::{LayerId, Matcher, Origin};
use routers_codec::osm::{OsmEdgeMetadata, OsmNetwork};
use routers_fixtures::{LAX_LYNWOOD_TRIP, LOS_ANGELES, VENTURA_TRIP, fixture};
use routers_network::Metadata;
use wkt::TryFromWkt;

const WARMUP: usize = 20;

fn main() {
    let cold = std::env::args().any(|arg| arg == "cold");
    let iterations = if cold { 60 } else { 300 };

    let graph = OsmNetwork::from_pbf(fixture!(LOS_ANGELES)).expect("graph must load");
    let runtime = OsmEdgeMetadata::default_runtime();
    let costing = CostingStrategies::default();

    println!(
        "{} cache, {iterations} matches each, mean per match\n",
        if cold { "cold" } else { "warm" }
    );

    for (name, trip) in [
        ("VENTURA_HWY", VENTURA_TRIP),
        ("LAX_LYNWOOD", LAX_LYNWOOD_TRIP),
    ] {
        let line = LineString::<f64>::try_from_wkt_str(trip).expect("trip must parse");
        let origins = line
            .into_points()
            .into_iter()
            .enumerate()
            .map(|(index, point)| Origin::new(point, index as i64))
            .collect::<Vec<_>>();

        let mut cache = Arc::new(PredicateCache::<OsmNetwork>::default());
        let mut phases = [Duration::ZERO; 4];
        let (mut searches, mut shape) = (0, (0, 0, 0));

        for iteration in 0..WARMUP + iterations {
            if cold {
                cache = Arc::new(PredicateCache::default());
            }

            let generator = StandardGenerator::new(&graph, &costing.emission);
            let weigher = AllCompute::default().use_cache(cache.clone());
            let matcher = Matcher::new(&graph, &costing, generator, weigher, &runtime);
            let mut trip = matcher.begin();

            let searched = cache.searches();
            let started = Instant::now();

            matcher.extend(&mut trip, &origins).expect("trip anchors");
            let generated = Instant::now();

            matcher.solve(&mut trip).expect("trip solves");
            let solved = Instant::now();

            let collapsed = matcher
                .snapshot(&mut trip)
                .expect("trip collapses")
                .into_owned();
            let collapse = Instant::now();

            let routed = RoutedPath::new(collapsed, &graph);
            let resolved = Instant::now();
            std::hint::black_box(routed);

            if iteration < WARMUP {
                continue;
            }

            phases[0] += generated - started;
            phases[1] += solved - generated;
            phases[2] += collapse - solved;
            phases[3] += resolved - collapse;
            searches += cache.searches() - searched;

            let layer = |index: usize| trip.layer(LayerId(index as u32)).map_or(0, <[_]>::len);
            shape = (
                trip.layers(),
                trip.candidates().len(),
                (1..trip.layers())
                    .map(|index| layer(index - 1) * layer(index))
                    .sum::<usize>(),
            );
        }

        let mean = |total: Duration| total.as_secs_f64() * 1e6 / iterations as f64;
        let (layers, candidates, hops) = shape;

        println!("{name}: {layers} points, {candidates} candidates, {hops} hops");
        for (phase, spent) in ["generate", "solve", "collapse", "resolve"]
            .iter()
            .zip(phases)
        {
            println!("  {phase:<9} {:>9.1} µs", mean(spent));
        }
        println!("  {:<9} {:>9.1} µs", "total", mean(phases.iter().sum()));
        println!("  searches  {:>9.1}\n", searches as f64 / iterations as f64);
    }
}
