//! Raw journal plane. (T05)
//!
//! Like [`partition`](crate::partition), everything here is wire law.
//! Producers choose a vehicle's subject, and every revision downstream is a
//! raw stream sequence — so the partition→subject and partition→stream
//! mappings must never move without a coordinated migration. In particular,
//! moving a partition to a different stream resets its sequence domain and
//! breaks every revision comparison across the boundary: the stream count is
//! fixed fleet-wide config, not a tunable.
//!
//! Retention is [`Limits`](async_nats::jetstream::stream::RetentionPolicy::Limits),
//! not work-queue: the journal is the replay source. An orchestrator recovers
//! by re-reading from just past its committed frontier, so an ack must *not*
//! delete the message it acknowledged — acks only advance the consumer, and
//! [`DiscardPolicy::Old`](async_nats::jetstream::stream::DiscardPolicy::Old)
//! ages messages out by [`max_age`](RawConfig::max_age) once they fall behind
//! every consumer's window. The streams are split across several raft leaders
//! (one stream is one leader) and each consumer filters exactly one partition
//! subject, which needs the non-overlapping filtered consumers NATS 2.10 added.

use core::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream::{
    self,
    consumer::{AckPolicy, DeliverPolicy, PullConsumer, pull},
    stream::{Config, DiscardPolicy, RetentionPolicy, StorageType},
};

use super::{DUPLICATE_WINDOW, create_or_update_stream};
use crate::partition::PARTITIONS;

/// Subject prefix for partitioned raw events: a vehicle's events publish to
/// `events.raw.p.<splitmix64(vehicle_id) % PARTITIONS>`.
pub const RAW_PREFIX: &str = "events.raw.p";

/// Retention and delivery knobs for the raw journal. Layout (`streams`) and
/// retention (`max_age`) belong to the stream; the backlog window
/// (`max_ack_pending`) and redelivery timeout (`ack_wait`) belong to a
/// partition's consumer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawConfig {
    /// How many raw streams the partition space divides across, fleet-wide.
    /// A caller derives a partition's stream from this same count (see
    /// [`raw_stream_index`]); it is fixed config, not a per-call tunable.
    pub streams: u64,
    /// How long a raw message is retained before ageing out. The journal is a
    /// replay source, so this bounds how far back recovery can rewind.
    pub max_age: Duration,
    /// Unacknowledged messages a partition's consumer may hold — the backlog
    /// knob. Under saturation the stream buffers and this throttles delivery;
    /// nothing is dropped.
    pub max_ack_pending: i64,
    /// How long the broker waits for an ack before redelivering — the
    /// crash-recovery timeout.
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

/// Which raw stream holds `partition`, out of `streams` total: contiguous
/// ranges, so a stream's subjects read as one block.
pub fn raw_stream_index(partition: u64, streams: u64) -> u64 {
    partition / PARTITIONS.div_ceil(streams)
}

/// The name of one raw stream.
pub fn raw_stream_name(index: u64) -> String {
    format!("EVENTS-RAW-{index}")
}

/// Idempotently reconcile the raw stream `index` of `streams`.
///
/// Retention is `Limits` with [`DiscardPolicy::Old`] so acks no longer delete
/// (see the module docs), which is a change from the earlier work-queue
/// journal — an already-provisioned stream must therefore be *updated*, which
/// is why this goes through [`create_or_update_stream`] rather than
/// `get_or_create_stream`. `streams` fixes the layout; the retention window is
/// [`RawConfig::max_age`].
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
/// `start` is the recovery seam. `None` binds (or creates) the durable at its
/// existing position — steady state. `Some(seq)` resumes strictly from `seq`
/// via [`DeliverPolicy::ByStartSequence`] (callers pass their committed
/// frontier + 1). A durable's deliver policy is immutable once set, so a
/// changed start cannot be applied in place: the consumer is dropped first and
/// recreated at the new sequence. That is safe precisely because the journal
/// retains acked messages — re-reading from the frontier replays exactly the
/// uncommitted tail and nothing already committed.
pub async fn raw_consumer(
    stream: &jetstream::stream::Stream,
    partition: u64,
    config: &RawConfig,
    start: Option<u64>,
) -> anyhow::Result<PullConsumer> {
    let name = format!("orchestrator-p{partition}");

    let deliver_policy = match start {
        Some(start_sequence) => {
            // The deliver policy is fixed at creation; recreate to move it.
            // Best-effort delete: an absent consumer is the fresh-recovery case.
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

    /// Every partition maps into exactly one stream, streams cover contiguous
    /// blocks, and the whole space is covered — for any stream count.
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
