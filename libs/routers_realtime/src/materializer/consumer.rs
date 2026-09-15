//! The materialiser consume loop: apply then acknowledge. (T28)
//!
//! [`run`] is the materialiser's whole runtime: pull committed output off a
//! [`Source`], apply each to the [`Sink`], and only *then* acknowledge it. That
//! order is the durability contract — an output is never acked before it is
//! persisted, so a crash between apply and ack redelivers it and the idempotent
//! merge (see [`sink`](super::sink)) absorbs the replay. The loop owns nothing
//! of the orchestrator's state; it rebuilds the served view purely from the
//! output plane, and so recovers its own consumption independently.
//!
//! Three outcomes, three responses:
//!
//! * **Applied.** The output persisted; acknowledge it and move on.
//! * **Sink error.** Persisting failed; negatively acknowledge with a short
//!   backoff so the broker redelivers it — never ack work that did not land.
//! * **Poison.** The source could not produce a decoded output (an undecodable
//!   message, or a transient pull error). A durable source retires an
//!   undecodable message itself before surfacing the error, so redriving it
//!   would only re-fail; the loop logs it, counts it, and carries on.

use core::time::Duration;

use routers_network::Entry;
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

use crate::bus::adapter::{AckHandle, Source};
use crate::lifecycle::Shutdown;
use crate::materializer::sink::{Applied, Sink};
use crate::protocol::output::CommittedOutput;

/// How long to hold back a redelivery after a sink error. Short: a failing sink
/// is usually a transient store hiccup, and the output must land promptly.
const NAK_BACKOFF: Duration = Duration::from_secs(1);

/// A tally of what one consumer run did, for logs and tests. Every
/// acknowledged output increments exactly one of `applied`, `duplicates`, or
/// `retracted`; `errors` and `poison` count the messages that were not applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Outputs that changed the view (an insert, supersede, reset, or terminal).
    pub applied: u64,
    /// Outputs that were an equal or lower revision — a replay or losing race.
    pub duplicates: u64,
    /// Outputs that dropped non-final layers.
    pub retracted: u64,
    /// Messages the source could not decode (or a transient pull error).
    pub poison: u64,
    /// Outputs the sink failed to persist; each was negatively acknowledged.
    pub errors: u64,
}

impl Stats {
    /// Fold one apply result into the tally.
    fn record(&mut self, applied: Applied) {
        match applied {
            Applied::Duplicate => self.duplicates += 1,
            Applied::Retracted { .. } => self.retracted += 1,
            Applied::Inserted { .. }
            | Applied::Superseded { .. }
            | Applied::SegmentOpened
            | Applied::Terminal => self.applied += 1,
        }
    }
}

/// Run the materialiser consume loop until shutdown or the source drains.
///
/// Each iteration races the shutdown signal against the next delivery
/// ([`Source::next`] is cancellation-safe, so losing that race drops the future
/// without consuming a message). Returns the run's [`Stats`]; an error is only
/// returned if acknowledging fails irrecoverably, which the caller treats as
/// fatal.
pub async fn run<E, S, Src>(mut source: Src, sink: S, shutdown: Shutdown) -> anyhow::Result<Stats>
where
    E: Entry + DeserializeOwned,
    S: Sink<E>,
    Src: Source<CommittedOutput<E>>,
{
    let mut stats = Stats::default();

    loop {
        tokio::select! {
            // Bias to shutdown: once triggered the loop stops before pulling
            // more work, even under a hot redelivery stream.
            biased;
            () = shutdown.triggered() => break,
            next = source.next() => {
                let delivery = match next {
                    // Source drained and closed: nothing more will arrive.
                    None => break,
                    Some(Ok(delivery)) => delivery,
                    Some(Err(err)) => {
                        // Poison: the durable source has already retired an
                        // undecodable message, so there is nothing to ack here.
                        warn!(error = %err, "skipping undecodable committed output");
                        stats.poison += 1;
                        continue;
                    }
                };

                match sink.apply(&delivery.item).await {
                    Ok(applied) => {
                        stats.record(applied);
                        if let Err(err) = delivery.handle.ack().await {
                            warn!(error = %err, "could not acknowledge applied output");
                        } else {
                            debug!(?applied, "applied committed output");
                        }
                    }
                    Err(err) => {
                        // Persisting failed: never ack unpersisted work. Ask for
                        // a backoff redelivery and carry on.
                        warn!(error = %err, "sink failed to apply output; naking");
                        stats.errors += 1;
                        if let Err(nak_err) =
                            delivery.handle.nak(Some(NAK_BACKOFF)).await
                        {
                            warn!(error = %nak_err, "could not nak failed output");
                        }
                    }
                }
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_network::{DirectionAwareEdgeId, Edge};

    use crate::bus::memory::MemoryBus;
    use crate::event::{MatchedDiff, MatchedLayer, VehicleId};
    use crate::lifecycle::{DrainReason, Shutdown};
    use crate::materializer::memory::MemorySink;
    use crate::materializer::sink::Applied;
    use crate::protocol::ids::headers;
    use crate::protocol::ids::{JobId, ObservationId, Revision, SegmentId};
    use crate::protocol::output::{CommittedOutput, OutputKind};

    use async_nats::HeaderMap;

    type E = MockEntryId;

    fn matched_job(job: u128, vehicle: u64, revision: u64, timestamp: i64) -> CommittedOutput<E> {
        let diff = MatchedDiff {
            revision,
            downgraded: false,
            layers: vec![MatchedLayer {
                timestamp,
                edge: Edge {
                    source: MockEntryId(1),
                    target: MockEntryId(2),
                    weight: 1,
                    id: DirectionAwareEdgeId::new(MockEntryId(3)),
                },
                position: Point::new(0.0, 0.0),
                path: Vec::new(),
            }],
        };
        CommittedOutput::new(
            JobId(job),
            VehicleId(vehicle),
            ObservationId {
                partition: 0,
                sequence: revision,
            },
            Revision(revision),
            SegmentId(1),
            OutputKind::Matched {
                diff,
                finalized_through: None,
            },
        )
    }

    fn matched(vehicle: u64, revision: u64, timestamp: i64) -> CommittedOutput<E> {
        matched_job(u128::from(revision), vehicle, revision, timestamp)
    }

    const FILTER: &str = "events.matched.v1.p.>";

    async fn publish(bus: &MemoryBus, output: &CommittedOutput<E>) {
        use crate::bus::adapter::Publisher;
        bus.publisher::<CommittedOutput<E>>()
            .publish(
                &output.subject(),
                &output.msg_id(),
                HeaderMap::new(),
                output,
            )
            .await
            .expect("publish");
    }

    #[tokio::test]
    async fn processes_and_acks_three_outputs() {
        let bus = MemoryBus::new();
        let source = bus.source::<CommittedOutput<E>>(FILTER);
        let sink = MemorySink::<E>::new();

        for vehicle in 1..=3 {
            // Distinct jobs per vehicle, so their output ids (and thus msg ids)
            // differ and the bus does not dedup them on the shared stream group.
            publish(&bus, &matched_job(u128::from(vehicle), vehicle, 5, 100)).await;
        }
        // Close so the source drains the three and then returns `None`.
        bus.close();

        let stats = run::<E, _, _>(source, sink.clone(), Shutdown::new())
            .await
            .expect("run");

        assert_eq!(stats.applied, 3);
        assert_eq!(stats.duplicates, 0);
        assert_eq!(bus.acked_count(FILTER), 3, "every output was acked");
        assert_eq!(sink.len(), 3, "every vehicle materialised");
    }

    #[tokio::test]
    async fn an_equal_revision_output_counts_as_a_duplicate() {
        let bus = MemoryBus::new();
        let source = bus.source::<CommittedOutput<E>>(FILTER);
        let sink = MemorySink::<E>::new();

        // Two outputs for the same (vehicle, timestamp) at the same revision but
        // from different jobs: distinct msg ids, so both are delivered, yet the
        // second merges to a duplicate.
        publish(&bus, &matched_job(1, 1, 5, 100)).await;
        publish(&bus, &matched_job(2, 1, 5, 100)).await;
        bus.close();

        let stats = run::<E, _, _>(source, sink, Shutdown::new())
            .await
            .expect("run");

        assert_eq!(stats.applied, 1);
        assert_eq!(stats.duplicates, 1);
        assert_eq!(bus.acked_count(FILTER), 2, "both were acked");
    }

    /// A sink that always fails; it trips the shared shutdown on its first apply
    /// so the loop stops right after the nak instead of spinning on redelivery.
    #[derive(Clone)]
    struct FailingSink {
        shutdown: Shutdown,
    }

    #[derive(Debug, thiserror::Error)]
    #[error("sink is down")]
    struct SinkDown;

    impl Sink<E> for FailingSink {
        type Error = SinkDown;

        async fn apply(&self, _output: &CommittedOutput<E>) -> Result<Applied, SinkDown> {
            self.shutdown.trigger(DrainReason::Operator);
            Err(SinkDown)
        }
    }

    #[tokio::test]
    async fn a_failing_sink_naks_and_does_not_ack() {
        let bus = MemoryBus::new();
        let source = bus.source::<CommittedOutput<E>>(FILTER);
        let shutdown = Shutdown::new();
        let sink = FailingSink {
            shutdown: shutdown.clone(),
        };

        publish(&bus, &matched(1, 5, 100)).await;

        let stats = run::<E, _, _>(source, sink, shutdown).await.expect("run");

        assert_eq!(stats.errors, 1, "the failed apply was counted");
        assert_eq!(stats.applied, 0);
        assert_eq!(bus.acked_count(FILTER), 0, "nothing was acked");
        assert_eq!(
            bus.nak_delays(),
            vec![Some(NAK_BACKOFF)],
            "the output was naked with the backoff",
        );
    }

    #[tokio::test]
    async fn shutdown_ends_the_loop() {
        let bus = MemoryBus::new();
        let source = bus.source::<CommittedOutput<E>>(FILTER);
        let sink = MemorySink::<E>::new();
        let shutdown = Shutdown::new();

        // No messages and the bus stays open, so the only way out is shutdown.
        let handle = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move { run::<E, _, _>(source, sink, shutdown).await })
        };
        shutdown.trigger(DrainReason::Signal);

        let stats = handle.await.expect("join").expect("run");
        assert_eq!(stats, Stats::default(), "no work was done");
    }

    #[tokio::test]
    async fn undecodable_message_is_counted_as_poison() {
        use crate::bus::adapter::Publisher;

        let bus = MemoryBus::new();
        let source = bus.source::<CommittedOutput<E>>(FILTER);
        let sink = MemorySink::<E>::new();

        // Raw bytes that are not a `CommittedOutput`: the source's decode fails.
        let mut poison_headers = HeaderMap::new();
        headers::stamp_schema(&mut poison_headers);
        bus.publisher::<CommittedOutput<E>>()
            .publish_bytes(
                &crate::topology::output::output_subject(0),
                "poison-1",
                poison_headers,
                b"not a committed output",
            )
            .await
            .expect("publish bytes");
        // A valid output behind it, so the loop keeps going past the poison.
        publish(&bus, &matched(1, 5, 100)).await;
        bus.close();

        let stats = run::<E, _, _>(source, sink.clone(), Shutdown::new())
            .await
            .expect("run");

        assert_eq!(stats.poison, 1);
        assert_eq!(stats.applied, 1, "the good output still applied");
        assert_eq!(sink.len(), 1);
    }
}
