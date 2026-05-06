use geo::Point;
use geo::{Coord, Distance, Haversine, LineString};
use lapin::options::{BasicAckOptions, BasicNackOptions};
use routers::{Match, PredicateCache};
use routers_realtime::event::Payload;
use routers_realtime::{Topic, TopicOpts};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use routers::r#match::MatchOptions;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{SYDNEY, SYDNEY_SAVED, fixture};
use routers_network::Metadata;

#[tokio::main]
async fn main() {
    let opts = TopicOpts::default().with_queue("queue.process");
    let mut topic = Topic::new(opts).await.expect("Unable to create topic");

    let mut map = HashMap::<String, VecDeque<Coord<f64>>>::new();

    let pbf_path = fixture!(SYDNEY);
    let saved_path = fixture!(SYDNEY_SAVED);

    if !saved_path.exists() {
        let graph = OsmNetwork::from_pbf(pbf_path).expect("Graph must be created");
        graph.save_to_file(saved_path).expect("must save to file");
    }

    let graph = OsmNetwork::from_saved(saved_path).expect("Graph must be created");
    let cache = Arc::new(PredicateCache::<OsmEntryId, OsmEdgeMetadata, OsmNetwork>::default());
    let runtime = OsmEdgeMetadata::default_runtime();

    while let event = topic.recv().await {
        match event {
            Ok(delivery) if let Ok(fmt) = serde_json::from_slice::<Payload>(&delivery.data) => {
                _ = delivery.ack(BasicAckOptions::default()).await;

                let context = map.entry(fmt.trip_id).or_default();
                context.push_back(fmt.point);

                let mut cumulative_dist = 0.0;
                let mut keep_from_index = 0;

                for i in (1..context.len()).rev() {
                    let current_pt = Point::from(context[i]);
                    let prev_pt = Point::from(context[i - 1]);

                    cumulative_dist += Haversine.distance(current_pt, prev_pt);

                    if cumulative_dist > 500.0 {
                        // Over 500m, discard.
                        keep_from_index = i;
                        break;
                    }
                }

                if keep_from_index > 0 {
                    context.drain(0..keep_from_index);
                }

                let opts = MatchOptions::new()
                    .with_runtime(runtime.clone())
                    .with_cache(cache.clone());

                let coords = context.into_iter().map(|c| c.clone()).collect::<Vec<_>>();
                let linestring = LineString::from(coords);

                let now = Instant::now();

                match graph.r#match(linestring, opts) {
                    Ok(_) => {
                        println!(
                            "{:?}: {:?} (Len={}) SUCCESS. ({:?})",
                            fmt.event_time,
                            fmt.point,
                            context.len(),
                            now.elapsed()
                        );
                    }
                    Err(_) => {
                        println!(
                            "{:?}: {:?} (Len={}) FAILED. ({:?})",
                            fmt.event_time,
                            fmt.point,
                            context.len(),
                            now.elapsed()
                        );
                    }
                }
            }
            Ok(delivery) => {
                _ = delivery.nack(BasicNackOptions::default()).await;
                eprintln!(
                    "Message does not match schema: {:?}",
                    String::from_utf8(delivery.data)
                );
            }
            Err(err) => {
                eprintln!("Encountered error in receive: {:?}", err);
            }
        }
    }

    topic.finish().await.expect("Failed to finish topic");
}
