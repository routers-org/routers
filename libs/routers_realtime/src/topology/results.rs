//! Partitioned solve-result plane.
//!
//! A matcher publishes each [`SolveResult`](crate::protocol::result::SolveResult)
//! back on the partition of the vehicle it solved, subject
//! `solve-result.v1.p.<partition>`, so the orchestrator that owns that partition
//! reads it. A result is not destroyed on ack, so `max_age` bounds its lifetime.

use core::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream::{
    self,
    consumer::{AckPolicy, PullConsumer, pull},
    stream::{Config, RetentionPolicy, StorageType},
};

use super::{create_or_update_stream, duplicate_window};

/// Subject prefix for partitioned solve results.
pub const RESULT_PREFIX: &str = "solve-result.v1.p";

/// The single stream retaining every partition's solve results.
pub const RESULT_STREAM: &str = "SOLVE-RESULTS";

/// Retention and delivery knobs for the result plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultsConfig {
    /// How long an unread result survives; bounds how far an orchestrator may lag.
    pub max_age: Duration,
    /// Unacknowledged results a partition's consumer may hold.
    pub max_ack_pending: i64,
    /// How long the broker waits for an ack before redelivering.
    pub ack_wait: Duration,
}

impl Default for ResultsConfig {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(10 * 60),
            // Results are consumed by one durable per partition; a small
            // broker-side window prevents 1,024 consumers from collectively
            // claiming an unbounded working set.
            max_ack_pending: 8,
            ack_wait: Duration::from_secs(30),
        }
    }
}

/// The solve-result subject of one partition.
pub fn result_subject(partition: u64) -> String {
    format!("{RESULT_PREFIX}.{partition}")
}

/// Idempotently reconcile the result stream.
pub async fn ensure_result_stream(
    context: &jetstream::Context,
    config: &ResultsConfig,
) -> anyhow::Result<jetstream::stream::Stream> {
    create_or_update_stream(
        context,
        Config {
            name: RESULT_STREAM.into(),
            subjects: vec![format!("{RESULT_PREFIX}.>")],
            retention: RetentionPolicy::Limits,
            storage: StorageType::File,
            max_age: config.max_age,
            duplicate_window: duplicate_window(config.max_age),
            ..Default::default()
        },
    )
    .await
    .context("could not reconcile result stream")
}

/// The durable consumer through which one partition's owner reads its results.
pub async fn result_consumer(
    stream: &jetstream::stream::Stream,
    partition: u64,
    config: &ResultsConfig,
) -> anyhow::Result<PullConsumer> {
    let name = format!("orchestrator-results-p{partition}");

    stream
        .get_or_create_consumer(
            &name,
            pull::Config {
                durable_name: Some(name.clone()),
                filter_subject: result_subject(partition),
                ack_policy: AckPolicy::Explicit,
                max_ack_pending: config.max_ack_pending,
                ack_wait: config.ack_wait,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("could not create result consumer for partition {partition}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_are_distinct_per_partition() {
        assert_eq!(result_subject(0), "solve-result.v1.p.0");
        assert_eq!(result_subject(485), "solve-result.v1.p.485");
        assert_eq!(result_subject(1023), "solve-result.v1.p.1023");
    }

    #[test]
    fn stream_name_is_stable() {
        assert_eq!(RESULT_STREAM, "SOLVE-RESULTS");
        assert_eq!(RESULT_PREFIX, "solve-result.v1.p");
    }

    #[test]
    fn default_delivery_window_is_partition_bounded() {
        assert_eq!(ResultsConfig::default().max_ack_pending, 8);
    }
}
