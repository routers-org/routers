//! The materialiser consume loop: apply then acknowledge.
//!
//! [`run`] pulls committed output off a [`Source`], applies each to the [`Sink`],
//! and only *then* acknowledges it. That order is the durability contract: an
//! output is never acked before it is persisted, so a crash between apply and ack
//! redelivers it and the idempotent merge absorbs the replay.

use alloc::collections::VecDeque;
use core::num::NonZeroUsize;
use core::time::Duration;
use std::collections::{HashMap, HashSet};

use futures::stream::{FuturesUnordered, StreamExt};
use routers_network::Entry;
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

use crate::bus::adapter::{AckHandle, Delivery, Source};
use crate::event::VehicleId;
use crate::lifecycle::Shutdown;
use crate::materializer::sink::{Applied, Sink};
use crate::metrics::Metrics;
use crate::protocol::output::CommittedOutput;

/// How long to hold back a redelivery after a sink error.
const NAK_BACKOFF: Duration = Duration::from_secs(1);

/// A tally of what one consumer run did, for logs and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Outputs that changed the view (an insert, supersede, reset, or terminal).
    pub applied: u64,
    /// Outputs that were an equal or lower revision.
    pub duplicates: u64,
    /// Outputs that dropped non-final layers.
    pub retracted: u64,
    /// Delivery errors surfaced by the source. Malformed wire payloads are
    /// acknowledged as transport poison before reaching this loop.
    pub poison: u64,
    /// Outputs the sink failed to persist; each was negatively acknowledged.
    pub errors: u64,
}

impl Stats {
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

enum Outcome {
    Applied(Applied),
    Failed,
}

enum Step<T, H: AckHandle> {
    Completed(VehicleId, Outcome),
    Delivery(Delivery<T, H>),
    SourceClosed,
    SourceError(anyhow::Error),
    Shutdown,
}

/// The bounded metric label for one [`Applied`] outcome.
fn applied_kind(applied: &Applied) -> &'static str {
    match applied {
        Applied::Inserted { .. } => "inserted",
        Applied::Superseded { .. } => "superseded",
        Applied::Duplicate => "duplicate",
        Applied::Retracted { .. } => "retracted",
        Applied::SegmentOpened => "segment_opened",
        Applied::Terminal => "terminal",
    }
}

/// Run the materialiser consume loop until shutdown or the source drains.
///
/// Equivalent to [`run_with_metrics`] with a [`Metrics::noop`] handle.
pub async fn run<E, S, Src>(source: Src, sink: S, shutdown: Shutdown) -> anyhow::Result<Stats>
where
    E: Entry + DeserializeOwned,
    S: Sink<E>,
    Src: Source<CommittedOutput<E>>,
{
    run_with_metrics(source, sink, shutdown, &Metrics::noop()).await
}

/// Run the materialiser consume loop until shutdown or the source drains,
/// recording bounded-label metrics through `metrics`.
///
/// Returns the run's [`Stats`]; an error is returned only if acknowledging fails
/// irrecoverably, which the caller treats as fatal.
pub async fn run_with_metrics<E, S, Src>(
    source: Src,
    sink: S,
    shutdown: Shutdown,
    metrics: &Metrics,
) -> anyhow::Result<Stats>
where
    E: Entry + DeserializeOwned,
    S: Sink<E>,
    Src: Source<CommittedOutput<E>>,
{
    run_concurrent_with_metrics(source, sink, shutdown, metrics, NonZeroUsize::MIN).await
}

/// Run the materialiser with at most `max_in_flight` storage operations.
///
/// Each operation still persists before acknowledging. This process applies
/// one vehicle's outputs in delivery order while other vehicles overlap I/O.
/// Cross-process races remain guarded by the sink's compare-and-set loop.
pub async fn run_concurrent_with_metrics<E, S, Src>(
    mut source: Src,
    sink: S,
    shutdown: Shutdown,
    metrics: &Metrics,
    max_in_flight: NonZeroUsize,
) -> anyhow::Result<Stats>
where
    E: Entry + DeserializeOwned,
    S: Sink<E>,
    Src: Source<CommittedOutput<E>>,
{
    let mut stats = Stats::default();
    let mut in_flight = FuturesUnordered::new();
    let mut active = HashSet::<VehicleId>::new();
    let mut pending =
        HashMap::<VehicleId, VecDeque<Delivery<CommittedOutput<E>, Src::Handle>>>::new();
    let mut queued = 0usize;
    let mut source_closed = false;

    while !source_closed || !in_flight.is_empty() {
        if shutdown.is_triggered() {
            source_closed = true;
        }
        if source_closed && in_flight.is_empty() {
            break;
        }
        let step = if source_closed || in_flight.len() + queued >= max_in_flight.get() {
            let (vehicle, outcome) = in_flight
                .next()
                .await
                .expect("queued work has an active apply");
            Step::Completed(vehicle, outcome)
        } else {
            tokio::select! {
                biased;
                () = shutdown.triggered() => Step::Shutdown,
                Some((vehicle, outcome)) = in_flight.next(), if !in_flight.is_empty() => {
                    Step::Completed(vehicle, outcome)
                }
                next = source.next() => match next {
                    Some(Ok(delivery)) => Step::Delivery(delivery),
                    Some(Err(error)) => Step::SourceError(error),
                    None => Step::SourceClosed,
                },
            }
        };

        match step {
            Step::Completed(vehicle, outcome) => {
                record_outcome(&mut stats, outcome);
                active.remove(&vehicle);
                if let Some(waiting) = pending.get_mut(&vehicle) {
                    if let Some(delivery) = waiting.pop_front() {
                        queued -= 1;
                        active.insert(vehicle);
                        in_flight.push(apply_keyed_one(vehicle, delivery, &sink, metrics));
                    }
                    if waiting.is_empty() {
                        pending.remove(&vehicle);
                    }
                }
            }
            Step::Delivery(delivery) => {
                let vehicle = delivery.item.vehicle_id;
                if active.insert(vehicle) {
                    in_flight.push(apply_keyed_one(vehicle, delivery, &sink, metrics));
                } else {
                    pending.entry(vehicle).or_default().push_back(delivery);
                    queued += 1;
                }
            }
            Step::SourceClosed | Step::Shutdown => source_closed = true,
            Step::SourceError(err) => {
                warn!(error = %err, "committed-output source failed; continuing");
                stats.poison += 1;
            }
        }
    }

    Ok(stats)
}

async fn apply_keyed_one<E, S, H>(
    vehicle: VehicleId,
    delivery: Delivery<CommittedOutput<E>, H>,
    sink: &S,
    metrics: &Metrics,
) -> (VehicleId, Outcome)
where
    E: Entry + DeserializeOwned,
    S: Sink<E>,
    H: AckHandle,
{
    (vehicle, apply_one(delivery, sink, metrics).await)
}

fn record_outcome(stats: &mut Stats, outcome: Outcome) {
    match outcome {
        Outcome::Applied(applied) => stats.record(applied),
        Outcome::Failed => stats.errors += 1,
    }
}

async fn apply_one<E, S, H>(
    delivery: Delivery<CommittedOutput<E>, H>,
    sink: &S,
    metrics: &Metrics,
) -> Outcome
where
    E: Entry + DeserializeOwned,
    S: Sink<E>,
    H: AckHandle,
{
    match sink.apply(&delivery.item).await {
        Ok(applied) => {
            metrics.materialized(applied_kind(&applied));
            if let Err(err) = delivery.handle.ack().await {
                warn!(error = %err, "could not acknowledge applied output");
            } else {
                debug!(?applied, "applied committed output");
            }
            Outcome::Applied(applied)
        }
        Err(err) => {
            // Never ack unpersisted work; nak for backoff redelivery.
            warn!(error = %err, "sink failed to apply output; naking");
            if let Err(nak_err) = delivery.handle.nak(Some(NAK_BACKOFF)).await {
                warn!(error = %nak_err, "could not nak failed output");
            }
            Outcome::Failed
        }
    }
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

    struct BlockingSink {
        arrivals: std::sync::atomic::AtomicUsize,
        barrier: std::sync::Arc<tokio::sync::Barrier>,
    }

    impl Sink<E> for BlockingSink {
        type Error = core::convert::Infallible;

        async fn apply(&self, _output: &CommittedOutput<E>) -> Result<Applied, Self::Error> {
            self.arrivals
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.barrier.wait().await;
            Ok(Applied::Duplicate)
        }
    }

    #[tokio::test]
    async fn concurrent_runner_overlaps_sink_io_up_to_its_bound() {
        const CAPACITY: usize = 4;

        let bus = MemoryBus::new();
        for vehicle in 1..=CAPACITY as u64 {
            publish(&bus, &matched_job(u128::from(vehicle), vehicle, 5, 100)).await;
        }
        bus.close();

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CAPACITY + 1));
        let sink = BlockingSink {
            arrivals: std::sync::atomic::AtomicUsize::new(0),
            barrier: std::sync::Arc::clone(&barrier),
        };
        let source = bus.source::<CommittedOutput<E>>(FILTER);
        let metrics = Metrics::noop();
        let run = run_concurrent_with_metrics::<E, _, _>(
            source,
            sink,
            Shutdown::new(),
            &metrics,
            NonZeroUsize::new(CAPACITY).unwrap(),
        );

        let (stats, _) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(run, barrier.wait())
        })
        .await
        .expect("storage operations were serialized");
        let stats = stats.expect("run");

        assert_eq!(stats.duplicates, CAPACITY as u64);
        assert_eq!(bus.acked_count(FILTER), CAPACITY);
    }

    #[derive(Clone, Default)]
    struct OrderedSink {
        seen: std::sync::Arc<std::sync::Mutex<Vec<(VehicleId, Revision)>>>,
        active: std::sync::Arc<std::sync::Mutex<HashSet<VehicleId>>>,
    }

    impl Sink<E> for OrderedSink {
        type Error = core::convert::Infallible;

        async fn apply(&self, output: &CommittedOutput<E>) -> Result<Applied, Self::Error> {
            {
                let mut active = self.active.lock().expect("active lock");
                assert!(
                    active.insert(output.vehicle_id),
                    "same vehicle applied concurrently"
                );
            }
            tokio::task::yield_now().await;
            self.seen
                .lock()
                .expect("seen lock")
                .push((output.vehicle_id, output.revision));
            self.active
                .lock()
                .expect("active lock")
                .remove(&output.vehicle_id);
            Ok(Applied::Duplicate)
        }
    }

    #[tokio::test]
    async fn concurrent_runner_serializes_each_vehicle_in_delivery_order() {
        let bus = MemoryBus::new();
        for (job, vehicle, revision) in [(1, 1, 1), (2, 2, 1), (3, 1, 2), (4, 1, 3)] {
            publish(&bus, &matched_job(job, vehicle, revision, revision as i64)).await;
        }
        bus.close();
        let sink = OrderedSink::default();
        let stats = run_concurrent_with_metrics::<E, _, _>(
            bus.source::<CommittedOutput<E>>(FILTER),
            sink.clone(),
            Shutdown::new(),
            &Metrics::noop(),
            NonZeroUsize::new(4).expect("nonzero capacity"),
        )
        .await
        .expect("run");

        let seen = sink.seen.lock().expect("seen lock");
        let revisions: Vec<_> = seen
            .iter()
            .filter(|(vehicle, _)| *vehicle == VehicleId(1))
            .map(|(_, revision)| revision.0)
            .collect();
        assert_eq!(revisions, [1, 2, 3]);
        assert_eq!(stats.duplicates, 4);
        assert_eq!(bus.acked_count(FILTER), 4);
    }

    #[tokio::test]
    async fn processes_and_acks_three_outputs() {
        let bus = MemoryBus::new();
        let source = bus.source::<CommittedOutput<E>>(FILTER);
        let sink = MemorySink::<E>::new();

        for vehicle in 1..=3 {
            // Distinct jobs per vehicle so msg ids differ and the bus does not dedup.
            publish(&bus, &matched_job(u128::from(vehicle), vehicle, 5, 100)).await;
        }
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

        // Same (vehicle, timestamp, revision) from different jobs: both delivered.
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

    /// A sink that always fails and trips the shared shutdown on its first apply.
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
    async fn malformed_payload_is_acked_by_the_transport_before_materializing() {
        use crate::bus::adapter::Publisher;

        let bus = MemoryBus::new();
        let source = bus.source::<CommittedOutput<E>>(FILTER);
        let sink = MemorySink::<E>::new();

        // Raw bytes that are not a `CommittedOutput`: the adapter owns their
        // decode and acknowledges this poison before it reaches the consumer.
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
        publish(&bus, &matched(1, 5, 100)).await;
        bus.close();

        let stats = run::<E, _, _>(source, sink.clone(), Shutdown::new())
            .await
            .expect("run");

        assert_eq!(
            stats.poison, 0,
            "transport poison does not leak into the reducer"
        );
        assert_eq!(stats.applied, 1, "the good output still applied");
        assert_eq!(sink.len(), 1);
        assert_eq!(
            bus.acked_count(FILTER),
            2,
            "the poison and good output were retired"
        );
    }
}
