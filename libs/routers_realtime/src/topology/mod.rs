//! The four message planes inside NATS JetStream.
//!
//! Each plane is a family of subjects backed by a stream and read through a
//! durable consumer: [`raw`] (the partitioned journal), [`jobs`] (the regional
//! solve-job work queue), [`results`] (partitioned solve results), and
//! [`output`] (committed matched output). Subject and stream names are wire
//! law: every process must derive them identically, so the builders and parsers
//! here are the single source of truth.

use core::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream;

use crate::partition::PARTITIONS;

pub mod jobs;
pub mod output;
pub mod raw;
pub mod results;

pub use jobs::*;
pub use output::*;
pub use raw::*;
pub use results::*;

/// The broker-side `Nats-Msg-Id` deduplication window on every stream.
pub const DUPLICATE_WINDOW: Duration = Duration::from_secs(2 * 60);

/// The partition a `.p.<partition>`-suffixed subject addresses, if it is
/// well-formed and in range. Job-plane subjects key by lane and return [`None`].
pub fn partition_of_subject(subject: &str) -> Option<u16> {
    let token = subject.rsplit_once(".p.")?.1;
    // Reject a trailing tail or any sign/padding a builder never produces.
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let partition: u16 = token.parse().ok()?;
    (u64::from(partition) < PARTITIONS).then_some(partition)
}

/// Idempotently reconcile a stream to `config`, returning its handle.
async fn create_or_update_stream(
    context: &jetstream::Context,
    config: jetstream::stream::Config,
) -> anyhow::Result<jetstream::stream::Stream> {
    let name = config.name.clone();
    match context.update_stream(&config).await {
        Ok(_) => context
            .get_stream(&name)
            .await
            .with_context(|| format!("could not open stream {name}")),
        Err(_) => context
            .create_stream(config)
            .await
            .with_context(|| format!("could not create stream {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_of_subject_reads_every_partitioned_plane() {
        for partition in [0u64, 1, 485, PARTITIONS - 1] {
            let expect = Some(partition as u16);
            assert_eq!(partition_of_subject(&raw::raw_subject(partition)), expect);
            assert_eq!(
                partition_of_subject(&results::result_subject(partition)),
                expect
            );
            assert_eq!(
                partition_of_subject(&output::output_subject(partition)),
                expect
            );
        }
    }

    #[test]
    fn partition_of_subject_rejects_the_unaddressable() {
        let rejected = [
            "solve.v1.g.europe.r.syd.q.0", // job plane keys by lane, not partition
            "events.raw.p",                // no partition token
            "events.raw.p.",               // empty token
            "events.raw.p.1024",           // == PARTITIONS, out of range
            "events.raw.p.99999",          // overflows u16
            "events.raw.p.3.oops",         // trailing tail
            "events.raw.p.-1",             // signed
            "events.raw.p.+1",             // padded sign
            "events.raw.p.0x1",            // not decimal
            "",                            // empty subject
        ];
        for subject in rejected {
            assert_eq!(
                partition_of_subject(subject),
                None,
                "{subject:?} should not resolve to a partition"
            );
        }
    }
}
