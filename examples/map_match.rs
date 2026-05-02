use geo::LineString;
use routers::r#match::MatchSimpleExt;
use routers_codec::osm::OsmNetwork;
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

    let mut match_times = vec![];

    for _ in 0..10_000 {
        let match_start = std::time::Instant::now();

        let _ = graph
            .r#match_simple(coordinates.clone())
            .expect("Match must complete successfully");

        match_times.push(match_start.elapsed().as_millis());
    }

    let avg_match_time = match_times.iter().sum::<u128>() / match_times.len() as u128;
    println!("Average match time: {}ms", avg_match_time);
}
