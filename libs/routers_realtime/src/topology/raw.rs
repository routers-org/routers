//! Raw journal plane.
//!
//! Like [`partition`](crate::partition), everything here is wire law: the
//! partition→subject and partition→stream mappings must not move without a
//! coordinated migration. Retention is `Limits`, not work-queue: the journal is
//! the replay source, so an ack advances a consumer without deleting messages.

use core::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream::{
    self, ErrorCode,
    consumer::{AckPolicy, DeliverPolicy, PullConsumer, pull},
    stream::{Config, ConsumerErrorKind, DiscardPolicy, RetentionPolicy, StorageType},
};

use super::{create_or_update_stream, duplicate_window};
use crate::partition::{PARTITIONS, ShardCount, ShardId};

/// Subject prefix for partitioned raw events: `events.raw.p.<partition>`.
pub const RAW_PREFIX: &str = "events.raw.p";

/// Retention and delivery knobs for the raw journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawConfig {
    /// How many raw streams the partition space divides across, fleet-wide.
    pub streams: u64,
    /// How long a raw message is retained before ageing out.
    pub max_age: Duration,
    /// Per-partition unacknowledged allowance within its shard consumer.
    pub max_ack_pending: i64,
    /// How long the broker waits for an ack before redelivering.
    pub ack_wait: Duration,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            streams: 4,
            max_age: Duration::from_secs(15 * 60),
            // This is a per-partition allowance. A sharded durable multiplies
            // it by its number of filtered partitions below.
            max_ack_pending: 8,
            ack_wait: Duration::from_secs(60),
        }
    }
}

/// The durable consumer owning one shard's raw events on one raw stream.
///
/// `partitions` must all be in this stream; the caller's validated shard plan
/// enforces that. A single delivery policy applies to the durable, so recovery
/// recreates it at the earliest member-partition seam. Workers suppress the
/// harmless replay before their own persisted frontier.
pub async fn raw_shard_consumer(
    stream: &jetstream::stream::Stream,
    stream_index: u64,
    shard_count: ShardCount,
    shard: ShardId,
    partitions: &[u16],
    config: &RawConfig,
    start: Option<u64>,
) -> anyhow::Result<PullConsumer> {
    anyhow::ensure!(
        !partitions.is_empty(),
        "raw shard {shard} has no partitions"
    );
    anyhow::ensure!(
        shard_count.partitions(shard).eq(partitions.iter().copied()),
        "raw shard {shard} does not contain exactly the configured partitions"
    );
    anyhow::ensure!(
        partitions.iter().all(
            |partition| raw_stream_index(u64::from(*partition), config.streams) == stream_index
        ),
        "raw shard {shard} crosses or does not belong to stream {stream_index}"
    );
    let name = raw_shard_consumer_name(stream_index, shard_count, shard);
    let deliver_policy = match start {
        Some(start_sequence) => {
            if let Err(err) = stream.delete_consumer(&name).await
                && !is_consumer_not_found(&err)
            {
                return Err(anyhow::Error::new(err)).with_context(|| {
                    format!("could not delete raw shard consumer {name} before moving it")
                });
            }
            DeliverPolicy::ByStartSequence { start_sequence }
        }
        None => DeliverPolicy::All,
    };
    let partition_count = i64::try_from(partitions.len())
        .context("raw shard partition count does not fit max_ack_pending")?;
    let max_ack_pending = config
        .max_ack_pending
        .checked_mul(partition_count)
        .filter(|pending| *pending > 0)
        .context("raw shard max_ack_pending must be positive and not overflow")?;
    let filters = partitions
        .iter()
        .map(|partition| raw_subject(u64::from(*partition)))
        .collect();
    let mut consumer = stream
        .get_or_create_consumer(
            &name,
            pull::Config {
                durable_name: Some(name.clone()),
                filter_subjects: filters,
                ack_policy: AckPolicy::Explicit,
                max_ack_pending,
                ack_wait: config.ack_wait,
                deliver_policy,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("could not create raw shard consumer {name}"))?;
    let actual = consumer
        .info()
        .await
        .with_context(|| format!("could not read raw shard consumer {name}"))?
        .config
        .deliver_policy;
    if actual != deliver_policy {
        anyhow::bail!(
            "raw shard consumer {name} has deliver policy {actual:?}, expected {deliver_policy:?}"
        );
    }
    Ok(consumer)
}

/// Stable raw durable name for a shard layout.
///
/// The shard count distinguishes deliberate layout migrations, while the lack
/// of a pod ordinal lets replica-count changes move a shard without losing its
/// broker frontier.
pub fn raw_shard_consumer_name(
    stream_index: u64,
    shard_count: ShardCount,
    shard: ShardId,
) -> String {
    format!(
        "orchestrator-raw-n{}-s{stream_index}-sh{shard}",
        shard_count.get()
    )
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
            duplicate_window: duplicate_window(config.max_age),
            ..Default::default()
        },
    )
    .await
    .with_context(|| format!("could not reconcile raw stream {index}"))
}

/// The durable consumer owning one partition's raw events.
///
/// `start` is the recovery seam: `None` binds the durable at its existing
/// position, `Some(seq)` recreates it to resume strictly from `seq`.
pub async fn raw_consumer(
    stream: &jetstream::stream::Stream,
    partition: u64,
    config: &RawConfig,
    start: Option<u64>,
) -> anyhow::Result<PullConsumer> {
    let name = format!("orchestrator-p{partition}");

    let deliver_policy = match start {
        Some(start_sequence) => {
            // A missing consumer is benign fresh recovery; any other delete failure is fatal.
            if let Err(err) = stream.delete_consumer(&name).await
                && !is_consumer_not_found(&err)
            {
                return Err(anyhow::Error::new(err)).with_context(|| {
                    format!("could not delete consumer for partition {partition} before moving it")
                });
            }
            DeliverPolicy::ByStartSequence { start_sequence }
        }
        None => DeliverPolicy::All,
    };

    let mut consumer = stream
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
        .with_context(|| format!("could not create consumer for partition {partition}"))?;

    // A stale durable's immutable deliver policy would replay from the wrong point; refuse it.
    let actual = consumer
        .info()
        .await
        .with_context(|| format!("could not read consumer info for partition {partition}"))?
        .config
        .deliver_policy;
    if actual != deliver_policy {
        anyhow::bail!(
            "partition {partition} consumer has deliver policy {actual:?}, expected {deliver_policy:?}"
        );
    }

    Ok(consumer)
}

/// Whether a delete error is the benign "consumer not found" case (JetStream code 10014).
fn is_consumer_not_found(err: &async_nats::jetstream::stream::ConsumerError) -> bool {
    matches!(
        err.kind(),
        ConsumerErrorKind::JetStream(js) if js.error_code() == ErrorCode::CONSUMER_NOT_FOUND
    )
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

    #[test]
    fn default_delivery_window_is_partition_bounded() {
        assert_eq!(RawConfig::default().max_ack_pending, 8);
    }

    #[test]
    fn sharded_durable_name_is_replica_independent_and_layout_scoped() {
        let shards = ShardCount::new(64).unwrap();
        let shard = shards.shard(7).unwrap();
        assert_eq!(
            raw_shard_consumer_name(0, shards, shard),
            "orchestrator-raw-n64-s0-sh7"
        );
    }
}
