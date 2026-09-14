//! The four message planes inside NATS JetStream. (T05)
//!
//! Every stage of the pipeline talks over one of four planes, and each plane
//! is a family of subjects backed by a stream and read through a durable
//! consumer:
//!
//! - [`raw`] — the partitioned raw journal an orchestrator replays from.
//! - [`jobs`] — the regional solve-job work queue matchers pull from.
//! - [`results`] — the partitioned solve results the owning orchestrator reads.
//! - [`output`] — the committed matched output the materialiser and observers
//!   consume.
//!
//! Subject and stream names are wire law, exactly like [`partition`](crate::partition):
//! producers and consumers in every process and release must derive them
//! identically, so the builders and parsers here are the single source of
//! truth. The parsers exist so a receiver can re-derive a message's partition
//! from the subject it arrived on and reject an envelope whose declared
//! identity does not match where it landed.
//!
//! The async functions that create streams and consumers are intentionally
//! thin: there is no broker in a unit test, so they are exercised by compile
//! only, and all the testable logic lives in the pure name builders and
//! parsers below.

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

/// The broker-side `Nats-Msg-Id` deduplication window applied to the job,
/// result, and output streams (and the raw journal). A publisher that retries
/// an ambiguous send within this window republishes byte-identical bytes under
/// the same id, and the broker collapses the duplicate rather than delivering
/// it twice.
pub const DUPLICATE_WINDOW: Duration = Duration::from_secs(2 * 60);

/// The partition a `.p.<partition>`-suffixed subject addresses, if it is
/// well-formed and in range.
///
/// The raw, result, and output planes all key their subjects by partition as a
/// trailing `.p.<partition>` token; this reads that token back. It is used to
/// validate an envelope's declared identity against the subject it was
/// delivered on, so it is deliberately strict: the token must be a bare
/// decimal number below [`PARTITIONS`], and there must be nothing after it.
/// The job plane keys by lane, not partition, so its subjects return [`None`].
pub fn partition_of_subject(subject: &str) -> Option<u16> {
    let token = subject.rsplit_once(".p.")?.1;
    // A partition is one final token: reject a trailing tail like
    // `events.raw.p.3.oops`, and reject signs or padding a builder never
    // produces so a spoofed subject cannot alias a real partition.
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let partition: u16 = token.parse().ok()?;
    (u64::from(partition) < PARTITIONS).then_some(partition)
}

/// Idempotently reconcile a stream to `config`, returning its handle.
///
/// async-nats 0.38 has no create-or-update primitive, and `get_or_create_stream`
/// will *not* update an existing stream's config — but this refactor changes
/// both the output plane's subjects (to versioned `events.matched.v1.p.>`) and
/// the raw journal's retention (to `Limits`), so an already-provisioned stream
/// must be reconciled, not left as-is. We update first (the steady state, where
/// the stream exists) and fall back to create when the stream is absent.
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
        // Each partitioned plane's builder round-trips through the shared
        // parser, for the boundary partitions.
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
