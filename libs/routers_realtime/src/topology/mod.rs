//! Subject and stream topology.
//!
//! [`raw`] and [`output`] are the durable JetStream journals; [`requests`] is
//! the transient solve request/reply plane. Subject and stream names must be
//! derived identically by every process.

use core::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream;

use crate::partition::PARTITIONS;

pub mod output;
pub mod raw;
pub mod requests;

pub use output::*;
pub use raw::*;
pub use requests::*;

/// The broker-side `Nats-Msg-Id` deduplication window on every stream.
pub const DUPLICATE_WINDOW: Duration = Duration::from_secs(2 * 60);

/// The window a stream can carry: JetStream rejects one longer than its `max_age`.
pub fn duplicate_window(max_age: Duration) -> Duration {
    if max_age.is_zero() {
        DUPLICATE_WINDOW
    } else {
        DUPLICATE_WINDOW.min(max_age)
    }
}

/// The partition a `.p.<partition>`-suffixed subject addresses, if it is
/// well-formed and in range. Request subjects key by lane and return [`None`].
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
    fn duplicate_window_fits_inside_max_age() {
        assert_eq!(
            duplicate_window(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
        assert_eq!(duplicate_window(Duration::from_secs(600)), DUPLICATE_WINDOW);
        assert_eq!(duplicate_window(Duration::ZERO), DUPLICATE_WINDOW);
    }

    #[test]
    fn partition_of_subject_reads_every_partitioned_plane() {
        for partition in [0u64, 1, 485, PARTITIONS - 1] {
            let expect = Some(partition as u16);
            assert_eq!(partition_of_subject(&raw::raw_subject(partition)), expect);
            assert_eq!(
                partition_of_subject(&requests::reply_subject("_INBOX.o", partition as u16)),
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
            "solve.req.v1.g.europe.r.syd.q.0", // requests key by lane, not partition
            "events.raw.p",                    // no partition token
            "events.raw.p.",                   // empty token
            "events.raw.p.1024",               // == PARTITIONS, out of range
            "events.raw.p.99999",              // overflows u16
            "events.raw.p.3.oops",             // trailing tail
            "events.raw.p.-1",                 // signed
            "events.raw.p.+1",                 // padded sign
            "events.raw.p.0x1",                // not decimal
            "",                                // empty subject
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
