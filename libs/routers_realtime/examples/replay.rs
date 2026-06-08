use chrono::NaiveDateTime;
use csv::ReaderBuilder;
use geo::Coord;
use routers_realtime::amqp::{Topic, TopicOpts};
use routers_realtime::event::{CsvReplayEvent, Payload};
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::{Instant, sleep_until};

const DEFAULT_CSV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/events.csv");
const TIME_FORMAT_MS: &str = "%Y-%m-%d %H:%M:%S%.f";
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

fn parse_event_time(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim_end_matches(" UTC");
    NaiveDateTime::parse_from_str(s, TIME_FORMAT_MS)
        .or_else(|_| NaiveDateTime::parse_from_str(s, TIME_FORMAT))
        .ok()
}

fn make_payload(record: CsvReplayEvent) -> Payload {
    Payload {
        trip_id: record.trip_id,
        vehicle_id: record.vehicle_id,
        provider: record.provider,
        event_time: record.event_time,
        point: Coord::from((record.longitude, record.latitude)),
    }
}

fn load_events(csv_path: &str) -> Result<Vec<(NaiveDateTime, CsvReplayEvent)>, Box<dyn Error>> {
    let file = File::open(csv_path)?;
    let reader = ReaderBuilder::new()
        .flexible(true)
        .has_headers(true)
        .from_reader(BufReader::new(file));

    let mut events: Vec<_> = reader
        .into_deserialize::<CsvReplayEvent>()
        .filter_map(|r| {
            let r = r.ok()?;
            let t = parse_event_time(&r.event_time)?;
            Some((t, r))
        })
        .collect();

    events.sort_by_key(|(t, _)| *t);
    Ok(events)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let csv_path = std::env::var("CSV_FILE").unwrap_or_else(|_| DEFAULT_CSV.to_string());
    let flood = std::env::var("REPLAY_FLOOD").is_ok();
    let speed: f64 = std::env::var("REPLAY_SPEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);

    let opts = TopicOpts::from_env().with_queue("queue.replay").with_auto_delete();

    println!("Loading CSV: {csv_path}");
    let events = load_events(&csv_path)?;
    if events.is_empty() {
        println!("No parseable events found.");
        return Ok(());
    }
    let total = events.len();

    if flood {
        flood_mode(opts, events).await?;
    } else {
        timed_mode(opts, events, speed).await?;
    }

    println!("Done — {total} events sent.");
    Ok(())
}

// Send as fast as the AMQP connection allows, no scheduling.
// Use this to saturate the orchestrator's input queue and measure max throughput.
async fn flood_mode(
    opts: TopicOpts,
    events: Vec<(NaiveDateTime, CsvReplayEvent)>,
) -> Result<(), Box<dyn Error>> {
    println!(
        "Connecting to RabbitMQ (FLOOD mode — {} events, no rate limit)...",
        events.len()
    );
    let topic = Topic::new(opts).await?;
    let total = events.len();
    let mut sent = 0u64;
    let t0 = std::time::Instant::now();

    for (_, record) in events {
        let bytes = serde_json::to_vec(&make_payload(record))?;
        if topic.send(&bytes).await.is_ok() {
            sent += 1;
        }
        if sent % 5000 == 0 {
            let rate = sent as f64 / t0.elapsed().as_secs_f64();
            print!("\r  flooded {sent}/{total}  ({rate:.0} msg/s publish rate)   ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }

    let elapsed = t0.elapsed().as_secs_f64();
    println!(
        "\r  Flooded {sent} events in {elapsed:.1}s  ({:.0} msg/s avg publish rate)",
        sent as f64 / elapsed
    );
    // Send AMQP Connection.Close and wait for Close-Ok before dropping the
    // socket. Then yield briefly so the OS can finish the TCP teardown
    // (FIN/FIN-ACK) before the process moves on. Without the sleep, the
    // kernel may still have unread data in the receive buffer when the socket
    // is freed, which causes it to send RST instead of FIN — crashing socat
    // and taking down every other connection on the same kubectl port-forward.
    let _ = topic.finish().await;
    // Wait for TCP FIN/ACK to complete before the process exits. Without this,
    // the OS may send RST instead of FIN if the socket is dropped with buffered
    // data, which crashes the kubectl port-forward socat process and severs the
    // orchestrator's connection.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    Ok(())
}

// Replay events at a wall-clock speed multiplier.
async fn timed_mode(
    opts: TopicOpts,
    events: Vec<(NaiveDateTime, CsvReplayEvent)>,
    speed: f64,
) -> Result<(), Box<dyn Error>> {
    println!("Connecting to RabbitMQ...");
    let topic = Arc::new(Topic::new(opts).await?);
    let total = events.len();
    let t0_event = events[0].0;
    let w0_wall = Instant::now();
    let sent = Arc::new(AtomicU64::new(0));

    println!(
        "Loaded {total} events spanning {:.1} min — replaying at {speed}×",
        (events.last().unwrap().0 - t0_event).num_seconds() as f64 / 60.0,
    );

    let mut handles = Vec::with_capacity(total);

    for (event_time, record) in events {
        let offset_secs = (event_time - t0_event).num_milliseconds() as f64 / 1000.0 / speed;
        let target = w0_wall + std::time::Duration::from_secs_f64(offset_secs.max(0.0));

        let topic = topic.clone();
        let sent = sent.clone();

        handles.push(tokio::spawn(async move {
            sleep_until(target).await;
            let bytes = serde_json::to_vec(&make_payload(record)).unwrap();
            if topic.send(&bytes).await.is_ok() {
                sent.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    let sent_ref = sent.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            ticker.tick().await;
            let n = sent_ref.load(Ordering::Relaxed);
            print!("\r  sent {n}/{total} ({:.1}%)   ", n as f64 / total as f64 * 100.0);
            use std::io::Write;
            let _ = std::io::stdout().flush();
            if n >= total as u64 { break; }
        }
    });

    for h in handles {
        let _ = h.await;
    }
    Ok(())
}
