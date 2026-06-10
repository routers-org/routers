use crate::{context::RawEvent, event::Payload};
use async_nats::jetstream::{self, stream::StorageType, Context};
use chrono::{DateTime, NaiveDateTime};
use futures::StreamExt;
use geo::Point;

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
                .unwrap_or(512 * 1024 * 1024),
        }
    }
}

/// Ensures the EVENTS JetStream stream exists with file storage.
/// Deletes and recreates the stream if the storage type needs to change.
pub async fn ensure_events_stream(
    js: &Context,
    opts: &NatsIngestOpts,
) -> anyhow::Result<()> {
    let config = jetstream::stream::Config {
        name: opts.stream_name.clone(),
        subjects: vec![opts.subject.clone()],
        storage: StorageType::File,
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

/// Connects to the EVENTS JetStream consumer and returns a stream of decoded
/// [`RawEvent`]s. Messages are acked immediately after decoding (or on parse
/// failure, to avoid poison-pill redelivery loops).
///
/// Multiple callers sharing the same `consumer_name` act as competing consumers:
/// NATS distributes messages across them.
pub async fn nats_source(
    js: Context,
    opts: NatsIngestOpts,
) -> anyhow::Result<impl futures::Stream<Item = RawEvent> + Send> {
    let stream = js
        .get_stream(&opts.stream_name)
        .await
        .map_err(|e| anyhow::anyhow!("get EVENTS stream: {e}"))?;
    let consumer = stream
        .get_or_create_consumer(
            &opts.consumer_name.clone(),
            jetstream::consumer::pull::Config {
                durable_name: Some(opts.consumer_name),
                filter_subject: opts.subject,
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
    Ok(messages.filter_map(|result| async move {
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
    }))
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
