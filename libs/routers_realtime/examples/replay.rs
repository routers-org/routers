use chrono::NaiveDateTime;
use csv::ReaderBuilder;
use geo::{Coord, Point};
use routers_realtime::event::{CsvReplayEvent, Payload};
use routers_realtime::{Topic, TopicOpts};
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tokio::time::{Instant, sleep_until};

const CSV_FILE_PATH: &str = "examples/events.csv";
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("Connecting to topic...");
    let opts = TopicOpts::default().with_queue("queue.replay");
    let topic = Arc::new(Topic::new(opts).await?);
    println!("Connected to topic.");

    println!("Loading and parsing CSV...");
    let file = File::open(CSV_FILE_PATH)?;
    let reader = BufReader::new(file);

    let reader = ReaderBuilder::new()
        .flexible(true)
        .has_headers(true)
        .from_reader(reader);

    println!("Reader created.");

    let mut events = reader
        .into_deserialize::<CsvReplayEvent>()
        .filter_map(|event| match event {
            Ok(event) => {
                match NaiveDateTime::parse_from_str(
                    event.event_time.trim_end_matches(" UTC"),
                    TIME_FORMAT,
                ) {
                    Ok(parsed) => Some((parsed, event)),
                    _ => None,
                }
            }
            Err(_) => None,
        })
        .collect::<Vec<_>>();

    if events.is_empty() {
        println!("No events found in CSV.");
        return Ok(());
    }

    // Sort globally by timestamp to guarantee chronological order
    // This fixes any grouping artifacts from the BigQuery export
    events.sort_by_key(|(time, _)| *time);

    let t0_event_time = events[0].0;
    let w0_wall_clock = Instant::now();

    println!(
        "Loaded {} events. Spawning async replay tasks...",
        events.len()
    );

    let mut handles = Vec::new();

    for (event_time, record) in events {
        // Calculate exact duration offset from the first event
        let offset = (event_time - t0_event_time).to_std().unwrap_or_default();
        let target_fire_time = w0_wall_clock + offset;

        let payload = Payload {
            trip_id: record.trip_id,
            vehicle_id: record.vehicle_id,
            provider: record.provider,
            event_time: record.event_time,
            point: Coord::from((record.longitude, record.latitude)),
        };

        let value = topic.clone();

        // Spawn a standalone task for EVERY event
        let handle = tokio::spawn(async move {
            // Task goes to sleep. Tokio's reactor will wake it at the exact Instant.
            sleep_until(target_fire_time).await;

            let payload_bytes = serde_json::to_vec(&payload).unwrap();
            let _ = value.send(&payload_bytes).await;
        });

        handles.push(handle);
    }

    println!(
        "All {} tasks scheduled. Replay in progress...",
        handles.len()
    );

    // Wait for all spawned tasks to finish
    for handle in handles {
        let _ = handle.await;
    }

    println!("Replay complete.");
    Ok(())
}
