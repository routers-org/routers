/// Polars-based replay for large CSV files.
///
/// Loads and sorts the full dataset with polars, then walks events in
/// chronological order, sleeping until each event's virtual time arrives
/// before publishing. This avoids spawning one async task per event, making
/// it viable for millions of rows.
///
/// Environment variables:
///   CSV_FILE        path to the CSV (default: sydney-dump-2026-thesis.csv)
///   RABBITMQ_URL    AMQP connection string
///   REPLAY_SPEED    speed multiplier (default: 1.0; try 60, 100, 0.5)
///   REPLAY_FLOOD    if set, ignore timing and publish as fast as possible
///   ACTIVE_SHARDS   comma-separated geohash list to filter by (e.g. r3grm,r3grh)
///                   if unset or empty, all events are sent
///   SHARD_PRECISION geohash precision used by the matcher (default: 5)
use chrono::NaiveDateTime;
use geo::{Coord, Point};
use polars::prelude::*;
use routers_realtime::amqp::{Topic, TopicOpts};
use routers_realtime::event::Payload;
use routers_shard::{Geohash, GeohashStrategy, ShardingStrategy};
use std::collections::HashSet;
use std::error::Error;
use std::io::Write;
use std::str::FromStr;
use tokio::time::Instant;

const DEFAULT_CSV: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/examples/sydney-dump-2026-thesis.csv");
const TIME_FORMAT_MS: &str = "%Y-%m-%d %H:%M:%S%.f";
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

fn parse_time(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim_end_matches(" UTC");
    NaiveDateTime::parse_from_str(s, TIME_FORMAT_MS)
        .or_else(|_| NaiveDateTime::parse_from_str(s, TIME_FORMAT))
        .ok()
}

/// Load the CSV with polars, sort by EventTime (lexicographic = chronological
/// for this ISO-like format), and return the pruned DataFrame.
fn load_sorted(path: &str) -> Result<DataFrame, PolarsError> {
    LazyCsvReader::new(path)
        .with_has_header(true)
        .finish()?
        .sort(["EventTime"], SortMultipleOptions::default())
        .select([
            col("TripID"),
            col("VehicleID"),
            col("Provider"),
            col("EventTime"),
            col("Latitude"),
            col("Longitude"),
        ])
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let csv_path = std::env::var("CSV_FILE").unwrap_or_else(|_| DEFAULT_CSV.to_string());
    let flood = std::env::var("REPLAY_FLOOD").is_ok();
    let speed: f64 = std::env::var("REPLAY_SPEED")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1.0);

    let shard_precision: u8 = std::env::var("SHARD_PRECISION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let strategy = GeohashStrategy::with_precision(shard_precision);

    let active_shards: HashSet<Geohash> = std::env::var("ACTIVE_SHARDS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|s| Geohash::from_str(s.trim()).ok())
        .collect();

    if flood {
        println!("Mode: flood (REPLAY_FLOOD set)");
    } else {
        println!("Mode: timed  speed={speed}×  (REPLAY_SPEED={:?})", std::env::var("REPLAY_SPEED").unwrap_or_else(|_| "unset — defaulting to 1.0".into()));
    }
    if active_shards.is_empty() {
        println!("Shard filter: none (sending all events)");
    } else {
        let mut names: Vec<_> = active_shards.iter().map(|s| s.to_string()).collect();
        names.sort();
        println!("Shard filter: {} shards — {}", names.len(), names.join(", "));
    }

    println!("Loading {}...", csv_path);
    let t_load = std::time::Instant::now();
    let df = load_sorted(&csv_path)?;
    let n = df.height();
    if n == 0 {
        println!("No events found.");
        return Ok(());
    }

    // Borrow all columns up front — column access is O(1).
    let trip_ids = df.column("TripID")?.str()?;
    let vehicle_ids = df.column("VehicleID")?.str()?;
    let providers = df.column("Provider")?.str()?;
    let event_times = df.column("EventTime")?.str()?;
    let latitudes = df.column("Latitude")?.f64()?;
    let longitudes = df.column("Longitude")?.f64()?;

    let t_first = parse_time(event_times.get(0).unwrap_or(""))
        .ok_or("could not parse first event time")?;
    let t_last = parse_time(event_times.get(n - 1).unwrap_or(""))
        .ok_or("could not parse last event time")?;
    let span_min = (t_last - t_first).num_seconds() as f64 / 60.0;

    println!(
        "Loaded {:>7} events spanning {:.1} min  ({:.2}s load+sort)",
        n,
        span_min,
        t_load.elapsed().as_secs_f64(),
    );

    let opts = TopicOpts::from_env()
        .with_queue("queue.replay")
        .with_auto_delete();
    println!("Connecting to RabbitMQ...");
    let topic = Topic::new(opts).await?;

    let effective_speed = if flood { f64::INFINITY } else { speed };
    if flood {
        println!("Flood mode — sending at max rate...");
    } else {
        println!(
            "Walking mode — replaying at {speed}× (wall span: {:.1} min)",
            span_min / speed,
        );
    }

    run(
        &topic,
        trip_ids,
        vehicle_ids,
        providers,
        event_times,
        latitudes,
        longitudes,
        n,
        t_first,
        effective_speed,
        &active_shards,
        &strategy,
    )
    .await?;

    let _ = topic.finish().await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    Ok(())
}

async fn run(
    topic: &Topic,
    trip_ids: &StringChunked,
    vehicle_ids: &StringChunked,
    providers: &StringChunked,
    event_times: &StringChunked,
    latitudes: &Float64Chunked,
    longitudes: &Float64Chunked,
    n: usize,
    t_first: NaiveDateTime,
    speed: f64,
    active_shards: &HashSet<Geohash>,
    strategy: &GeohashStrategy,
) -> Result<(), Box<dyn Error>> {
    let wall_start = Instant::now();
    let mut sent: u64 = 0;
    let mut skipped: u64 = 0;
    let mut last_report = Instant::now();
    let mut last_sent: u64 = 0;

    for i in 0..n {
        let time_str = event_times.get(i).unwrap_or("");
        let lat = latitudes.get(i).unwrap_or(0.0);
        let lon = longitudes.get(i).unwrap_or(0.0);

        // Drop events outside the active shard set before any timing logic so
        // they don't inflate the wall-clock timeline for filtered replays.
        if !active_shards.is_empty() {
            let shard = strategy.locate(Point::new(lon, lat));
            if !active_shards.contains(&shard) {
                skipped += 1;
                continue;
            }
        }

        // Compute the wall-clock instant this event should be dispatched.
        // If we are behind schedule (target already past), send immediately.
        if speed.is_finite() {
            if let Some(event_time) = parse_time(time_str) {
                let offset_ms = (event_time - t_first).num_milliseconds();
                let target_wall_ms = (offset_ms as f64 / speed).max(0.0) as u64;
                let target = wall_start + std::time::Duration::from_millis(target_wall_ms);
                if target > Instant::now() {
                    tokio::time::sleep_until(target).await;
                }
            }
        }

        let payload = Payload {
            trip_id: trip_ids.get(i).unwrap_or("").to_owned(),
            vehicle_id: vehicle_ids.get(i).unwrap_or("").to_owned(),
            provider: providers.get(i).unwrap_or("").to_owned(),
            event_time: time_str.to_owned(),
            point: Coord::from((lon, lat)),
        };
        if let Ok(bytes) = serde_json::to_vec(&payload) {
            if topic.send(&bytes).await.is_ok() {
                sent += 1;
            }
        }

        // Progress line every ~2s
        let now = Instant::now();
        if now.duration_since(last_report).as_secs_f64() >= 2.0 {
            let rate =
                (sent - last_sent) as f64 / now.duration_since(last_report).as_secs_f64();
            let pct = (sent + skipped) as f64 / n as f64 * 100.0;
            if skipped > 0 {
                print!("\r  [{pct:5.1}%]  sent={sent:>7}  skipped={skipped:>7}  {rate:>8.0} events/s   ");
            } else {
                print!("\r  [{pct:5.1}%]  {sent:>7}/{n}  {rate:>8.0} events/s   ");
            }
            let _ = std::io::stdout().flush();
            last_report = now;
            last_sent = sent;
        }
    }

    if skipped > 0 {
        println!("\r  Done — {sent} sent, {skipped} skipped (outside active shards) in {:.2}s", wall_start.elapsed().as_secs_f64());
    } else {
        println!("\r  Done — {sent} events sent in {:.2}s", wall_start.elapsed().as_secs_f64());
    }
    Ok(())
}
