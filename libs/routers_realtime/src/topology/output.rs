//! Committed matched-output plane. (T05)
//!
//! The orchestrator publishes each committed
//! [`CommittedOutput`](crate::protocol::output::CommittedOutput) on the
//! partition of its vehicle, subject `events.matched.v1.p.<partition>`. This is
//! the fan-out plane: the materialiser consumes it durably to build the served
//! view, and any number of observers tail it ephemerally. Subjects are keyed by
//! partition, so [`partition_of_subject`](super::partition_of_subject) can
//! check an output landed where its identity claims.
//!
//! The `v1` token is a real migration seam. The earlier plane published on
//! `events.matched.p.>`; this stream keeps the same name (`EVENTS-MATCHED`) but
//! moves to `events.matched.v1.p.>`. Because [`get_or_create_stream`] will not
//! rewrite an existing stream's subjects, provisioning goes through
//! [`create_or_update_stream`], which updates the live stream in place.
//!
//! [`get_or_create_stream`]: async_nats::jetstream::Context::get_or_create_stream

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
    /// How long committed output is retained. Nothing consumes it
    /// destructively and observers may lag, so size it for the materialiser's
    /// worst-case catch-up.
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
///
/// `Limits` retention on
/// [`File`](async_nats::jetstream::stream::StorageType::File) storage: output
/// ages out by time, never on consumption, so late observers and a lagging
/// materialiser both still see it. Uses [`create_or_update_stream`] so the
/// subject move to `events.matched.v1.p.>` is applied to an existing stream.
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

/// A durable pull consumer over the whole output plane. The materialiser binds
/// one (named `materializer`) so its ack marks an output applied to the served
/// view; observers that need replay can create their own by name.
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
