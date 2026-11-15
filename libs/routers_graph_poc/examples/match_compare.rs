//! Map matching over `OsmNetwork` against the same match over
//! `CompactNetwork`, plugged in unchanged — and checked to route identically.
//!
//! ```sh
//! POC_REGION=sydney cargo run --release -p routers_graph_poc --example match_compare
//! POC_NETWORK=compact cargo run --release -p routers_graph_poc --example match_compare   # one side only
//! ```
//!
//! `POC_NETWORK=osm|compact` runs one network alone, for extracts too large
//! to hold twice; it prints a digest of the route to compare across runs.

use core::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use geo::LineString;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{LAX_LYNWOOD_TRIP, LOS_ANGELES, SYDNEY, SYNDEY_TRIP, VENTURA_TRIP, fixture};
use routers_graph_poc::CompactNetwork;
use routers_network::{Entry, Metadata, Network};
use routers_transition::primitives::PredicateCache;
use routers_transition::uom::si::f64::Length;
use routers_transition::uom::si::length::meter;
use routers_transition::{Match, MatchOptions};
use wkt::TryFromWkt;

/// What one configuration measured, and the route it found.
struct Outcome {
    label: String,
    cold: Duration,
    warm: Duration,
    searches: u64,
    /// `(x, y, way)` per interpolated element, bit for bit.
    route: Vec<(u64, u64, i64)>,
}

impl Outcome {
    fn digest(&self) -> u64 {
        let mut hasher = rustc_hash::FxHasher::default();
        self.route.hash(&mut hasher);
        hasher.finish()
    }
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn measure<N>(network: &N, trips: &[(&'static str, &'static str, f64)]) -> Vec<Outcome>
where
    N: Network<
            Entry = OsmEntryId,
            Meta = OsmEdgeMetadata,
            Runtime = <OsmEdgeMetadata as Metadata>::Runtime,
        >,
{
    let runtime = OsmEdgeMetadata::default_runtime();

    trips
        .iter()
        .filter_map(|&(name, wkt, reach)| {
            let line = LineString::<f64>::try_from_wkt_str(wkt).expect("trip parses");
            let cache = || {
                Arc::new(PredicateCache::<N>::with_reach_distance(
                    Length::new::<meter>(reach),
                ))
            };
            let options = |cache: Arc<PredicateCache<N>>| {
                MatchOptions::new()
                    .with_runtime(runtime.clone())
                    .with_cache(cache)
            };

            // Cold: a fresh cache per match, so every root is searched.
            let mut cold = Vec::new();
            let mut searches = 0;
            for _ in 0..9 {
                let fresh = cache();
                let started = Instant::now();
                let routed = network.r#match(line.clone(), options(fresh.clone()));
                cold.push(started.elapsed());
                searches = fresh.searches();

                if let Err(error) = routed {
                    println!("  {name} (reach {reach} m): no match — {error}");
                    return None;
                }
            }

            // Warm: one cache kept across matches.
            let kept = cache();
            let mut warm = Vec::new();
            let mut route = Vec::new();
            // `POC_WARM` lengthens the warm run, for profiling it.
            let rounds = std::env::var("POC_WARM")
                .ok()
                .and_then(|rounds| rounds.parse().ok())
                .unwrap_or(60);
            for _ in 0..rounds {
                let started = Instant::now();
                let routed = network
                    .r#match(line.clone(), options(kept.clone()))
                    .expect("matched cold, so matches warm");
                warm.push(started.elapsed());

                route = routed
                    .interpolated
                    .elements
                    .iter()
                    .map(|element| {
                        (
                            element.point.x.to_bits(),
                            element.point.y.to_bits(),
                            element.edge.id().identifier(),
                        )
                    })
                    .collect();
            }

            Some(Outcome {
                label: format!("{name} (reach {reach:.0} m)"),
                cold: median(cold),
                warm: median(warm.split_off(10)),
                searches,
                route,
            })
        })
        .collect()
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

fn main() {
    let sydney = std::env::var("POC_REGION").is_ok_and(|region| region == "sydney");
    let only = std::env::var("POC_NETWORK").ok();

    let (pbf, trips) = if sydney {
        (
            fixture!(SYDNEY),
            vec![
                ("SYDNEY", SYNDEY_TRIP, 1_000.0),
                ("SYDNEY", SYNDEY_TRIP, 5_000.0),
            ],
        )
    } else {
        (
            fixture!(LOS_ANGELES),
            vec![
                ("VENTURA_HWY", VENTURA_TRIP, 1_000.0),
                ("LAX_LYNWOOD", LAX_LYNWOOD_TRIP, 1_000.0),
            ],
        )
    };

    let report = |network: &str, outcomes: &[Outcome]| {
        for outcome in outcomes {
            println!(
                "  {network:<8} {:<28} cold {:>8.2} ms   warm {:>7.3} ms   searches {:>5}   route {} elements, digest {:016x}",
                outcome.label,
                ms(outcome.cold),
                ms(outcome.warm),
                outcome.searches,
                outcome.route.len(),
                outcome.digest()
            );
        }
    };

    let osm = (only.as_deref() != Some("compact")).then(|| {
        let network = OsmNetwork::from_pbf(pbf).expect("pbf ingests");
        let outcomes = measure(&network, &trips);
        report("osm", &outcomes);
        outcomes
    });

    let compact = (only.as_deref() != Some("osm")).then(|| {
        let network = CompactNetwork::from_pbf(pbf).expect("pbf ingests");
        let outcomes = measure(&network, &trips);
        report("compact", &outcomes);
        outcomes
    });

    if let (Some(osm), Some(compact)) = (osm, compact) {
        println!();
        for (osm, compact) in osm.iter().zip(&compact) {
            println!(
                "  {:<28} cold {:>5.2}x   warm {:>5.2}x   identical route: {}",
                osm.label,
                osm.cold.as_secs_f64() / compact.cold.as_secs_f64(),
                osm.warm.as_secs_f64() / compact.warm.as_secs_f64(),
                osm.route == compact.route
            );
        }
    }
}
