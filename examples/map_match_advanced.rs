use std::sync::Arc;

use rayon::prelude::*;

use geo::LineString;
use routers::{
    DefaultEmissionCost, Match, PredicateCache,
    r#match::{MatchOptions, MatchSimpleExt},
};
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_network::Metadata;
use wkt::TryFromWkt;

use routers_fixtures::{SYDNEY, SYDNEY_SAVED, SYNDEY_TRIP, fixture};

fn main() {
    let prog_start = std::time::Instant::now();

    let coordinates: LineString<f64> =
        LineString::try_from_wkt_str(SYNDEY_TRIP).expect("must parse");

    println!("Setup time: {}ms", prog_start.elapsed().as_millis());
    let now = std::time::Instant::now();

    let pbf_path = fixture!(SYDNEY);
    let saved_path = fixture!(SYDNEY_SAVED);

    if !saved_path.exists() {
        let graph = OsmNetwork::from_pbf(pbf_path).expect("Graph must be created");
        graph.save_to_file(saved_path).expect("must save to file");
    }

    let graph = OsmNetwork::from_saved(saved_path).expect("Graph must be created");

    println!("Initialisation time: {}ms", now.elapsed().as_millis());

    let cache = Arc::new(PredicateCache::<OsmEntryId, OsmEdgeMetadata, OsmNetwork>::default());
    let runtime = OsmEdgeMetadata::default_runtime();

    let match_times = (0..1_000)
        .map(|_| {
            let match_start = std::time::Instant::now();

            let opts = MatchOptions::new()
                .with_runtime(runtime.clone())
                .with_cache(cache.clone());

            let _ = graph
                .r#match(coordinates.clone(), opts)
                .expect("Match must complete successfully");

            match_start.elapsed().as_micros() as usize
        })
        .collect::<Vec<_>>();

    let points = coordinates.0.len();

    let avg_match_time = match_times.iter().sum::<usize>() / match_times.len() as usize;
    println!(
        "Average match time: {} micros (Points={})",
        avg_match_time, points
    );
    println!(
        "Avg time per point: {} micros (Points per second: {})",
        avg_match_time / points,
        points as f64 / avg_match_time as f64 * 1_000_000.0
    );
}
