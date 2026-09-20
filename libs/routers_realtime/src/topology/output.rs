//! Committed matched-output plane.
//!
//! The orchestrator publishes each
//! [`CommittedOutput`](crate::protocol::output::CommittedOutput) on its vehicle's
//! partition, subject `events.matched.v1.p.<partition>`: the materialiser
//! consumes it durably and observers tail it ephemerally.

use core::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream::{
    self,
    consumer::{AckPolicy, PullConsumer, pull},
    stream::{Config, RetentionPolicy, StorageType},
};

use super::{DUPLICATE_WINDOW, create_or_update_stream};

/// Subject prefix for partitioned committed output.
pub const OUTPUT_PREFIX: &str = "events.matched.v1.p";

/// The single stream retaining committed matched output.
pub const OUTPUT_STREAM: &str = "EVENTS-MATCHED";

/// Retention knobs for the committed-output plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputConfig {
    /// How long committed output is retained.
    pub max_age: Duration,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(15 * 60),
        }
    }
}

/// The committed-output subject of one partition.
pub fn output_subject(partition: u64) -> String {
    format!("{OUTPUT_PREFIX}.{partition}")
}

/// Idempotently reconcile the committed-output stream.
pub async fn ensure_output_stream(
    context: &jetstream::Context,
    config: &OutputConfig,
) -> anyhow::Result<jetstream::stream::Stream> {
    create_or_update_stream(
        context,
        Config {
            name: OUTPUT_STREAM.into(),
            subjects: vec![format!("{OUTPUT_PREFIX}.>")],
            retention: RetentionPolicy::Limits,
            storage: StorageType::File,
            max_age: config.max_age,
            duplicate_window: DUPLICATE_WINDOW,
            ..Default::default()
        },
    )
    .await
    .context("could not reconcile committed-output stream")
}

/// A durable pull consumer over the whole output plane (the materialiser binds one named `materializer`).
pub async fn output_consumer(
    stream: &jetstream::stream::Stream,
    name: &str,
) -> anyhow::Result<PullConsumer> {
    stream
        .get_or_create_consumer(
            name,
            pull::Config {
                durable_name: Some(name.to_owned()),
                filter_subject: format!("{OUTPUT_PREFIX}.>"),
                ack_policy: AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await
        .with_context(|| format!("could not create output consumer {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_are_distinct_per_partition() {
        assert_eq!(output_subject(0), "events.matched.v1.p.0");
        assert_eq!(output_subject(485), "events.matched.v1.p.485");
        assert_eq!(output_subject(1023), "events.matched.v1.p.1023");
    }

    #[test]
    fn stream_name_and_prefix_are_stable() {
        assert_eq!(OUTPUT_STREAM, "EVENTS-MATCHED");
        assert_eq!(OUTPUT_PREFIX, "events.matched.v1.p");
    }
}
