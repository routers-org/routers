/// Polars-based replay for large CSV files.
///
/// Loads and sorts the full dataset with polars, then walks events in
/// chronological order, sleeping until each event's virtual time arrives
/// before publishing. Publishes directly to the NATS EVENTS JetStream stream.
///
/// Environment variables:
///   CSV_FILE          path to the CSV (default: sydney-dump-2026-thesis.csv)
///   NATS_URL          NATS connection string
///   REPLAY_SPEED      speed multiplier (default: 1.0; try 60, 100, 0.5)
///   REPLAY_FLOOD      if set, ignore timing and publish as fast as possible
///   REPLAY_PIPELINE   number of JetStream publishes to fire before awaiting acks
///                     (default: 1 = sequential; set to 256+ in flood mode)
///   REPLAY_LOOPS      how many times to cycle through the dataset (default: 1;
///                     0 = repeat indefinitely until killed)
///   ACTIVE_SHARDS     comma-separated geohash list to filter by (e.g. r3grm,r3grh)
///                     if unset or empty, all events are sent
///   SHARD_PRECISION   geohash precision used by the matcher (default: 5)
use async_nats::jetstream;
use chrono::NaiveDateTime;
use geo::{Coord, Point};
use polars::prelude::*;
use routers_realtime::event::Payload;
use routers_realtime::nats_ingest::{self, NatsIngestOpts};
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
    let loops: usize = std::env::var("REPLAY_LOOPS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1);
    let pipeline: usize = std::env::var("REPLAY_PIPELINE")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1)
        .max(1);
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
        println!("Mode: flood (REPLAY_FLOOD set)  pipeline={pipeline}");
    } else {
        println!(
            "Mode: timed  speed={speed}×  (REPLAY_SPEED={:?})",
            std::env::var("REPLAY_SPEED").unwrap_or_else(|_| "unset — defaulting to 1.0".into()),
        );
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

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
    println!("Connecting to NATS ({nats_url})...");
    let nc = async_nats::connect(&nats_url).await?;
    let js = async_nats::jetstream::new(nc);
    let opts = NatsIngestOpts::from_env();
    nats_ingest::ensure_events_stream(&js, &opts).await?;
    let subject = opts.subject.clone();
    println!("EVENTS stream ready — subject={subject}");

    let effective_speed = if flood { f64::INFINITY } else { speed };
    let loop_desc = |n: usize| if n == 0 { "∞".to_string() } else { n.to_string() };
    if flood {
        println!(
            "Flood mode — sending at max rate  pipeline={pipeline}  loops={}",
            loop_desc(loops),
        );
    } else {
        println!(
            "Walking mode — replaying at {speed}×  loops={}  (wall span per loop: {:.1} min)",
            loop_desc(loops),
            span_min / speed,
        );
    }

    let mut iteration = 0usize;
    loop {
        iteration += 1;
        if loops != 1 {
            let label = if loops == 0 {
                format!("loop {iteration}")
            } else {
                format!("loop {iteration}/{loops}")
            };
            println!("{label}");
        }

        run(
            &js,
            &subject,
            trip_ids,
            vehicle_ids,
            providers,
            event_times,
            latitudes,
            longitudes,
            n,
            t_first,
            effective_speed,
            pipeline,
            &active_shards,
            &strategy,
        )
        .await?;

        if loops > 0 && iteration >= loops {
            break;
        }
    }

    Ok(())
}

async fn run(
    js: &jetstream::Context,
    subject: &str,
    trip_ids: &StringChunked,
    vehicle_ids: &StringChunked,
    providers: &StringChunked,
    event_times: &StringChunked,
    latitudes: &Float64Chunked,
    longitudes: &Float64Chunked,
    n: usize,
    t_first: NaiveDateTime,
    speed: f64,
    pipeline: usize,
    active_shards: &HashSet<Geohash>,
    strategy: &GeohashStrategy,
) -> Result<(), Box<dyn Error>> {
    let wall_start = Instant::now();
    let mut sent: u64 = 0;
    let mut skipped: u64 = 0;
    let mut last_report = Instant::now();
    let mut last_sent: u64 = 0;

    let mut pending: Vec<jetstream::context::PublishAckFuture> = Vec::with_capacity(pipeline);

    for i in 0..n {
        let time_str = event_times.get(i).unwrap_or("");
        let lat = latitudes.get(i).unwrap_or(0.0);
        let lon = longitudes.get(i).unwrap_or(0.0);

        if !active_shards.is_empty() {
            let shard = strategy.locate(Point::new(lon, lat));
            if !active_shards.contains(&shard) {
                skipped += 1;
                continue;
            }
        }

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
        let Ok(bytes) = serde_json::to_vec(&payload) else { continue };

        if speed.is_infinite() && pipeline > 1 {
            match js.publish(subject.to_owned(), bytes.into()).await {
                Ok(ack) => { pending.push(ack); sent += 1; }
                Err(e) => eprintln!("\n[replay] publish error: {e}"),
            }
            if pending.len() >= pipeline {
                for a in pending.drain(..) { let _ = a.await; }
            }
        } else {
            match js.publish(subject.to_owned(), bytes.into()).await {
                Ok(ack) => { let _ = ack.await; sent += 1; }
                Err(e) => eprintln!("\n[replay] publish error: {e}"),
            }
        }

        let now = Instant::now();
        if now.duration_since(last_report).as_secs_f64() >= 1.0 {
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

    for a in pending.drain(..) { let _ = a.await; }

    if skipped > 0 {
        println!(
            "\r  Done — {sent} sent, {skipped} skipped (outside active shards) in {:.2}s",
            wall_start.elapsed().as_secs_f64(),
        );
    } else {
        println!(
            "\r  Done — {sent} events sent in {:.2}s",
            wall_start.elapsed().as_secs_f64(),
        );
    }
    Ok(())
}
