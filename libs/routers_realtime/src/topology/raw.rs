//! Raw journal plane.
//!
//! The partition-to-subject and partition-to-stream mappings are wire law and
//! require a coordinated migration to change. Acks advance consumers without
//! deleting messages so committed frontiers remain replayable.

use core::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream::{
    self,
    consumer::{AckPolicy, DeliverPolicy, PullConsumer, pull},
    stream::{Config, DiscardPolicy, RetentionPolicy, StorageType},
};

use super::{DUPLICATE_WINDOW, create_or_update_stream};
use crate::partition::PARTITIONS;

/// Subject prefix for partitioned raw events: `events.raw.p.<partition>`.
pub const RAW_PREFIX: &str = "events.raw.p";

/// Retention and delivery knobs for the raw journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawConfig {
    /// How many raw streams the partition space divides across, fleet-wide.
    pub streams: u64,
    /// How long a raw message is retained before ageing out.
    pub max_age: Duration,
    /// Unacknowledged messages a partition's consumer may hold.
    pub max_ack_pending: i64,
    /// How long the broker waits for an ack before redelivering.
    pub ack_wait: Duration,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            streams: 4,
            max_age: Duration::from_secs(15 * 60),
            max_ack_pending: 2048,
            ack_wait: Duration::from_secs(60),
        }
    }
}

/// The raw-event subject of one partition.
pub fn raw_subject(partition: u64) -> String {
    format!("{RAW_PREFIX}.{partition}")
}

/// Which raw stream holds `partition`, out of `streams` total; contiguous ranges.
pub fn raw_stream_index(partition: u64, streams: u64) -> u64 {
    partition / PARTITIONS.div_ceil(streams)
}

/// The name of one raw stream.
pub fn raw_stream_name(index: u64) -> String {
    format!("EVENTS-RAW-{index}")
}

/// Idempotently reconcile the raw stream `index` of `streams`.
pub async fn ensure_raw_stream(
    context: &jetstream::Context,
    index: u64,
    streams: u64,
    config: &RawConfig,
) -> anyhow::Result<jetstream::stream::Stream> {
    let chunk = PARTITIONS.div_ceil(streams);
    let partitions = (index * chunk)..(((index + 1) * chunk).min(PARTITIONS));

    create_or_update_stream(
        context,
        Config {
            name: raw_stream_name(index),
            subjects: partitions.map(raw_subject).collect(),
            retention: RetentionPolicy::Limits,
            storage: StorageType::File,
            max_age: config.max_age,
            discard: DiscardPolicy::Old,
            duplicate_window: DUPLICATE_WINDOW,
            ..Default::default()
        },
    )
    .await
    .with_context(|| format!("could not reconcile raw stream {index}"))
}

/// The durable consumer owning one partition's raw events.
///
/// `start` is the recovery seam: `None` binds the existing durable and
/// `Some(seq)` recreates it from that sequence.
pub async fn raw_consumer(
    stream: &jetstream::stream::Stream,
    partition: u64,
    config: &RawConfig,
    start: Option<u64>,
) -> anyhow::Result<PullConsumer> {
    let name = format!("orchestrator-p{partition}");

    let deliver_policy = match start {
        Some(start_sequence) => {
            // Deliver policy is immutable, so recreate the durable to move it.
            let _ = stream.delete_consumer(&name).await;
            DeliverPolicy::ByStartSequence { start_sequence }
        }
        None => DeliverPolicy::All,
    };

    stream
        .get_or_create_consumer(
            &name,
            pull::Config {
                durable_name: Some(name.clone()),
                filter_subject: raw_subject(partition),
                ack_policy: AckPolicy::Explicit,
                max_ack_pending: config.max_ack_pending,
                ack_wait: config.ack_wait,
                deliver_policy,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("could not create consumer for partition {partition}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_partition_the_partition_space() {
        for streams in [1u64, 2, 3, 4, 8, 32, 1024] {
            let mut last = 0;
            for partition in 0..PARTITIONS {
                let index = raw_stream_index(partition, streams);
                assert!(index < streams, "index {index} escapes {streams} streams");
                assert!(index >= last, "stream blocks must be contiguous");
                last = index;
            }
        }
    }

    #[test]
    fn subjects_are_distinct_per_partition() {
        assert_eq!(raw_subject(0), "events.raw.p.0");
        assert_eq!(raw_subject(485), "events.raw.p.485");
        assert_eq!(raw_subject(1023), "events.raw.p.1023");
    }

    #[test]
    fn stream_names_are_indexed() {
        assert_eq!(raw_stream_name(0), "EVENTS-RAW-0");
        assert_eq!(raw_stream_name(3), "EVENTS-RAW-3");
    }
}
