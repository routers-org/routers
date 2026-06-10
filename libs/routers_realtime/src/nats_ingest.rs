use crate::{context::RawEvent, event::Payload};
use async_nats::jetstream::{self, stream::StorageType, Context};
use chrono::{DateTime, NaiveDateTime};
use futures::{SinkExt, StreamExt};
use geo::Point;
use std::pin::Pin;
use std::time::Duration;

pub struct NatsIngestOpts {
    pub stream_name: String,
    pub subject: String,
    pub consumer_name: String,
    pub max_bytes: i64,
}

impl Default for NatsIngestOpts {
    fn default() -> Self {
        Self {
            stream_name: "EVENTS".into(),
            subject: "events.raw".into(),
            consumer_name: "orchestrator".into(),
            max_bytes: 512 * 1024 * 1024,
        }
    }
}

impl NatsIngestOpts {
    pub fn from_env() -> Self {
        Self {
            stream_name: std::env::var("EVENTS_STREAM")
                .unwrap_or_else(|_| "EVENTS".into()),
            subject: std::env::var("EVENTS_SUBJECT")
                .unwrap_or_else(|_| "events.raw".into()),
            consumer_name: std::env::var("EVENTS_CONSUMER")
                .unwrap_or_else(|_| "orchestrator".into()),
            max_bytes: std::env::var("EVENTS_MAX_BYTES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(128 * 1024 * 1024),
        }
    }
}

/// Ensures the EVENTS JetStream stream exists with memory storage.
/// Deletes and recreates the stream if the storage type needs to change.
pub async fn ensure_events_stream(
    js: &Context,
    opts: &NatsIngestOpts,
) -> anyhow::Result<()> {
    let config = jetstream::stream::Config {
        name: opts.stream_name.clone(),
        subjects: vec![opts.subject.clone()],
        storage: StorageType::Memory,
        max_bytes: opts.max_bytes,
        discard: jetstream::stream::DiscardPolicy::Old,
        ..Default::default()
    };
    match js.update_stream(&config).await {
        Ok(_) => {}
        Err(_) => {
            let _ = js.delete_stream(&opts.stream_name).await;
            js.create_stream(config)
                .await
                .map_err(|e| anyhow::anyhow!("NATS EVENTS stream create: {e}"))?;
        }
    }
    Ok(())
}

type RawEventStream = Pin<Box<dyn futures::Stream<Item = RawEvent> + Send>>;

/// Creates a single NATS pull consumer messages stream, decoding each message
/// into a [`RawEvent`]. Messages are acked immediately after decoding.
async fn create_nats_stream(
    js: &Context,
    opts: &NatsIngestOpts,
) -> anyhow::Result<RawEventStream> {
    let stream = js
        .get_stream(&opts.stream_name)
        .await
        .map_err(|e| anyhow::anyhow!("get EVENTS stream: {e}"))?;
    let consumer = stream
        .get_or_create_consumer(
            &opts.consumer_name,
            jetstream::consumer::pull::Config {
                durable_name: Some(opts.consumer_name.clone()),
                filter_subject: opts.subject.clone(),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("create EVENTS consumer: {e}"))?;
    let messages = consumer
        .messages()
        .await
        .map_err(|e| anyhow::anyhow!("EVENTS consumer messages: {e}"))?;
    Ok(Box::pin(messages.filter_map(|result| async move {
        let msg = result.ok()?;
        let payload: Option<Payload> = serde_json::from_slice(&msg.payload)
            .map_err(|e| eprintln!("[nats_source] JSON parse error: {e}"))
            .ok();
        let _ = msg.ack().await;
        let p = payload?;
        Some(RawEvent {
            vehicle_id: p.vehicle_id,
            coord: Point::new(p.point.x, p.point.y),
            timestamp_ms: parse_event_time(&p.event_time),
        })
    })))
}

/// Connects to the EVENTS JetStream consumer and returns an infinite stream of
/// decoded [`RawEvent`]s. Messages are acked immediately after decoding (or on
/// parse failure, to avoid poison-pill redelivery loops).
///
/// The returned stream is self-reconnecting: if the underlying NATS consumer
/// messages stream ends (e.g. after a NATS restart or a 404-no-messages cycle),
/// it transparently creates a new consumer and resumes. The caller never sees
/// a stream termination under normal conditions.
///
/// Multiple callers sharing the same `consumer_name` act as competing consumers:
/// NATS distributes messages across them.
pub async fn nats_source(
    js: Context,
    opts: NatsIngestOpts,
) -> anyhow::Result<impl futures::Stream<Item = RawEvent> + Send> {
    // Verify the stream and consumer are reachable before returning. The first
    // inner stream is passed directly to the background task to avoid a second
    // round-trip.
    let first = create_nats_stream(&js, &opts).await?;

    let (mut tx, rx) = futures::channel::mpsc::channel::<RawEvent>(256);

    tokio::spawn(async move {
        let mut pending: Option<RawEventStream> = Some(first);

        'outer: loop {
            let mut inner = match pending.take() {
                Some(s) => s,
                None => match create_nats_stream(&js, &opts).await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[nats_source] reconnect error: {e}, retrying in 1s");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue 'outer;
                    }
                },
            };

            loop {
                match inner.next().await {
                    Some(event) => {
                        if tx.send(event).await.is_err() {
                            return; // downstream dropped — stop task
                        }
                    }
                    None => {
                        eprintln!("[nats_source] consumer stream ended, reconnecting");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        break; // break inner loop, outer loop recreates consumer
                    }
                }
            }
        }
    });

    Ok(rx)
}

fn parse_event_time(s: &str) -> u64 {
    DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f %Z")
        .or_else(|_| DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S %Z"))
        .map(|dt| dt.timestamp_millis() as u64)
        .unwrap_or_else(|_| {
            let s = s.trim_end_matches(" UTC");
            NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
                .map(|dt| dt.and_utc().timestamp_millis() as u64)
                .unwrap_or(0)
        })
}
