//! Ordered raw reader: envelope validation and duplicate suppression.
//!
//! Classifies one raw partition delivery as a new observation to queue, poison
//! to ack-and-forget, or a duplicate to suppress. It performs no I/O and acks
//! nothing. Identity must agree across the delivery subject, the vehicle id's
//! partition, and the reader's own; a mismatch is poison.

use async_nats::HeaderMap;
use routers_network::Entry;
use tokio::time::Instant;

use crate::bus::Wire;
use crate::bus::adapter::{AckHandle, Delivery};
use crate::event::Payload;
use crate::ingress;
use crate::orchestrator::frontier::{FrontierTracker, Observed};
use crate::orchestrator::scheduler::{Enqueue, PendingObservation, Scheduler};
use crate::partition;
use crate::protocol::ids::{ObservationId, SCHEMA_VERSION, SchemaVersion, headers};
use crate::topology::partition_of_subject;

/// Why a raw message can never become an observation and is acked-and-forgotten.
///
/// Every variant is a permanent fault of this message, so the worker acks and
/// completes its sequence rather than looping. Labels are bounded for metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoisonReason {
    /// The payload bytes did not decode as a protobuf [`Payload`].
    Decode,
    /// The delivery subject's partition disagreed with the reader's (or was
    /// absent), so the message was misrouted.
    SubjectPartition {
        /// The partition the reader owns.
        expected: u16,
        /// The partition parsed from the subject, if any.
        got: Option<u16>,
    },
    /// The vehicle id hashes to a different partition than the one that
    /// delivered it — a producer that published to the wrong subject.
    VehiclePartition {
        /// The partition the message was delivered under (the reader's).
        subject: u16,
        /// The partition the vehicle id actually hashes to.
        vehicle: u16,
    },
    /// A stamped wire-schema header did not match this build's
    /// [`SCHEMA_VERSION`].
    Schema {
        /// The version a peer stamped, if one was present and well-formed.
        got: Option<u32>,
    },
    /// The payload failed a basic sanity check (zero vehicle id, non-finite or
    /// out-of-range coordinate).
    Invalid {
        /// The bounded [`ingress::IngressError::kind`] of the failing check.
        kind: &'static str,
    },
}

impl PoisonReason {
    /// A bounded, stable metric label for this reason.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            PoisonReason::Decode => "decode",
            PoisonReason::SubjectPartition { .. } => "subject_partition",
            PoisonReason::VehiclePartition { .. } => "vehicle_partition",
            PoisonReason::Schema { .. } => "schema",
            // Already a bounded `IngressError::kind`, so it doubles as the label.
            PoisonReason::Invalid { kind } => kind,
        }
    }
}

/// Why a valid raw message was suppressed rather than queued.
///
/// [`SuppressReason::Coalesced`] is the one reason that is *not* terminal — see
/// [`RawDisposition::is_terminal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuppressReason {
    /// The sequence is at or below the completion frontier — an already-terminal
    /// redelivery.
    BehindFrontier,
    /// The loaded checkpoint already covers this observation's input.
    Committed,
    /// The same observation id is already pending or in flight; the in-flight
    /// original owns its completion.
    Coalesced,
    /// The vehicle's pending backlog is full; the observation was refused.
    Overflow,
}

impl SuppressReason {
    /// A bounded, stable metric label for this reason.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            SuppressReason::BehindFrontier => "behind_frontier",
            SuppressReason::Committed => "committed",
            SuppressReason::Coalesced => "coalesced",
            SuppressReason::Overflow => "overflow",
        }
    }
}

/// The verdict on one raw delivery.
///
/// [`Queued`](RawDisposition::Queued) hands no ack handle back — the scheduler
/// owns it; the other variants return the handle for the worker to ack.
#[derive(Debug)]
#[must_use = "a disposition may carry an ack handle that must be acknowledged"]
pub enum RawDisposition<H: AckHandle> {
    /// The observation was appended to its vehicle's FIFO; the scheduler holds
    /// the handle.
    Queued {
        /// The vehicle's pending depth after the append.
        depth: usize,
    },
    /// The message can never be processed; ack it and complete its sequence.
    Poison {
        /// The handle to acknowledge the poison delivery.
        handle: H,
        /// Why it is poison.
        reason: PoisonReason,
    },
    /// The message is valid but already decided or a duplicate; ack it, and
    /// complete its sequence unless the reason is
    /// [`SuppressReason::Coalesced`].
    Suppressed {
        /// The handle to acknowledge the suppressed delivery.
        handle: H,
        /// Why it was suppressed.
        reason: SuppressReason,
    },
}

impl<H: AckHandle> RawDisposition<H> {
    /// Whether the worker should mark this message's sequence complete on the
    /// frontier after acking it.
    ///
    /// A [`SuppressReason::Coalesced`] or [`Queued`](RawDisposition::Queued)
    /// message is *not* terminal — its sequence is completed by the in-flight
    /// original or the later commit; everything else is terminal.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self {
            RawDisposition::Poison { .. } => true,
            RawDisposition::Suppressed { reason, .. } => {
                !matches!(reason, SuppressReason::Coalesced)
            }
            RawDisposition::Queued { .. } => false,
        }
    }
}

/// A raw message still in its wire form, for the path where decoding may fail.
pub struct RawEnvelope<'a, H: AckHandle> {
    /// The concrete subject the message was delivered on.
    pub subject: &'a str,
    /// The message headers, if any were carried (the schema header lives here).
    pub headers: Option<&'a HeaderMap>,
    /// The undecoded payload bytes.
    pub bytes: &'a [u8],
    /// The acknowledgement handle for this delivery.
    pub handle: H,
}

/// The per-partition raw reader.
///
/// It holds only its partition; the scheduler and frontier tracker are passed
/// in per call, so it is trivially shareable and every decision is pure.
#[derive(Clone, Copy, Debug)]
pub struct RawReader {
    partition: u16,
}

impl RawReader {
    /// A reader for `partition`.
    #[must_use]
    pub fn new(partition: u16) -> Self {
        Self { partition }
    }

    /// The partition this reader owns.
    #[must_use]
    pub fn partition(&self) -> u16 {
        self.partition
    }

    /// Classify an already-decoded delivery.
    ///
    /// A [`Delivery`] carries no headers, so the schema check is skipped as an
    /// absent (optional) header. Use [`RawReader::admit_bytes`] on the raw path.
    pub fn admit<E, H>(
        &self,
        scheduler: &mut Scheduler<E, H>,
        tracker: &mut FrontierTracker,
        delivery: Delivery<Payload, H>,
        now: Instant,
    ) -> RawDisposition<H>
    where
        E: Entry,
        H: AckHandle,
    {
        let got = partition_of_subject(&delivery.subject);
        if got != Some(self.partition) {
            return RawDisposition::Poison {
                handle: delivery.handle,
                reason: PoisonReason::SubjectPartition {
                    expected: self.partition,
                    got,
                },
            };
        }
        self.admit_payload(
            scheduler,
            tracker,
            None,
            delivery.item,
            delivery.handle,
            now,
        )
    }

    /// Classify a raw, still-encoded delivery, decoding it here so a decode
    /// failure poisons cleanly with the ack handle intact.
    pub fn admit_bytes<E, H>(
        &self,
        scheduler: &mut Scheduler<E, H>,
        tracker: &mut FrontierTracker,
        envelope: RawEnvelope<'_, H>,
        now: Instant,
    ) -> RawDisposition<H>
    where
        E: Entry,
        H: AckHandle,
    {
        let RawEnvelope {
            subject,
            headers,
            bytes,
            handle,
        } = envelope;

        let got = partition_of_subject(subject);
        if got != Some(self.partition) {
            return RawDisposition::Poison {
                handle,
                reason: PoisonReason::SubjectPartition {
                    expected: self.partition,
                    got,
                },
            };
        }

        let payload = match Payload::decode(bytes) {
            Ok(payload) => payload,
            Err(_) => {
                return RawDisposition::Poison {
                    handle,
                    reason: PoisonReason::Decode,
                };
            }
        };

        let schema = headers.and_then(headers::schema_of);
        self.admit_payload(scheduler, tracker, schema, payload, handle, now)
    }

    /// The shared tail: identity, schema, and sanity, then frontier and
    /// scheduler suppression (the subject is already validated).
    fn admit_payload<E, H>(
        &self,
        scheduler: &mut Scheduler<E, H>,
        tracker: &mut FrontierTracker,
        schema: Option<SchemaVersion>,
        payload: Payload,
        handle: H,
        now: Instant,
    ) -> RawDisposition<H>
    where
        E: Entry,
        H: AckHandle,
    {
        let vehicle_partition = partition::partition_of(payload.vehicle_id) as u16;
        if vehicle_partition != self.partition {
            return RawDisposition::Poison {
                handle,
                reason: PoisonReason::VehiclePartition {
                    subject: self.partition,
                    vehicle: vehicle_partition,
                },
            };
        }

        if let Some(version) = schema
            && version != SCHEMA_VERSION
        {
            return RawDisposition::Poison {
                handle,
                reason: PoisonReason::Schema {
                    got: Some(version.0),
                },
            };
        }

        if let Err(error) = ingress::validate(&payload) {
            return RawDisposition::Poison {
                handle,
                reason: PoisonReason::Invalid { kind: error.kind() },
            };
        }

        let sequence = handle.sequence();
        match tracker.observe(sequence) {
            Observed::BehindFrontier => {
                return RawDisposition::Suppressed {
                    handle,
                    reason: SuppressReason::BehindFrontier,
                };
            }
            Observed::Duplicate => {
                return RawDisposition::Suppressed {
                    handle,
                    reason: SuppressReason::Coalesced,
                };
            }
            Observed::New => {}
        }

        let observation = PendingObservation {
            id: ObservationId {
                partition: self.partition,
                sequence,
            },
            payload,
            handle,
            received: now,
        };
        match scheduler.enqueue(observation, now) {
            Enqueue::Queued { depth } => RawDisposition::Queued { depth },
            Enqueue::Coalesced(handle) => RawDisposition::Suppressed {
                handle,
                reason: SuppressReason::Coalesced,
            },
            Enqueue::Committed(handle) => RawDisposition::Suppressed {
                handle,
                reason: SuppressReason::Committed,
            },
            Enqueue::Overflow { handle, .. } => RawDisposition::Suppressed {
                handle,
                reason: SuppressReason::Overflow,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::time::Duration;
    use std::sync::Mutex;

    use chrono::{TimeZone, Utc};
    use geo::Point;
    use routers_network::mock::MockEntryId;

    use super::*;
    use crate::event::VehicleId;
    use crate::orchestrator::scheduler::{CheckpointState, SchedulerConfig};
    use crate::protocol::ids::{GraphVersion, RegionId, Revision, SCHEMA_VERSION, SegmentId};
    use crate::store::checkpoint::{PartitionFrontier, VehicleCheckpoint};

    use routers_transition::matcher::Trip;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum AckOp {
        Ack(u64),
        Nak(u64),
    }

    #[derive(Clone, Debug)]
    struct TestAck {
        seq: u64,
        log: Arc<Mutex<Vec<AckOp>>>,
    }

    impl AckHandle for TestAck {
        async fn ack(self) -> anyhow::Result<()> {
            self.log.lock().unwrap().push(AckOp::Ack(self.seq));
            Ok(())
        }

        async fn nak(self, _delay: Option<Duration>) -> anyhow::Result<()> {
            self.log.lock().unwrap().push(AckOp::Nak(self.seq));
            Ok(())
        }

        fn sequence(&self) -> u64 {
            self.seq
        }

        fn deliveries(&self) -> u32 {
            1
        }
    }

    /// Vehicle 1 hashes to partition 485 (pinned in `partition.rs`); tests rely
    /// on that pairing.
    const PARTITION: u16 = 485;
    const VEHICLE: u64 = 1;

    struct Fixture {
        log: Arc<Mutex<Vec<AckOp>>>,
        region: RegionId,
        now: Instant,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                log: Arc::new(Mutex::new(Vec::new())),
                region: RegionId::new("r1").unwrap(),
                now: Instant::now(),
            }
        }

        fn ack(&self, seq: u64) -> TestAck {
            TestAck {
                seq,
                log: self.log.clone(),
            }
        }

        fn payload(&self, vehicle: u64, lon: f64, lat: f64) -> Payload {
            Payload {
                vehicle_id: VehicleId(vehicle),
                timestamp: Utc.timestamp_micros(1_775_000_000_000_000).unwrap(),
                point: Point::new(lon, lat),
            }
        }

        fn good(&self) -> Payload {
            self.payload(VEHICLE, 151.2093, -33.8688)
        }

        fn envelope<'a>(
            &self,
            subject: &'a str,
            headers: Option<&'a HeaderMap>,
            bytes: &'a [u8],
            seq: u64,
        ) -> RawEnvelope<'a, TestAck> {
            RawEnvelope {
                subject,
                headers,
                bytes,
                handle: self.ack(seq),
            }
        }

        fn present_checkpoint(&self, last_seq: u64) -> CheckpointState<MockEntryId> {
            CheckpointState::Present(VehicleCheckpoint {
                trip: Trip::new(),
                last_input: ObservationId {
                    partition: PARTITION,
                    sequence: last_seq,
                },
                revision: Revision(last_seq),
                segment: SegmentId(1),
                finalized_through: None,
                graph: GraphVersion::new("g1").unwrap(),
                schema: SCHEMA_VERSION,
                region: self.region.clone(),
            })
        }
    }

    fn scheduler() -> Scheduler<MockEntryId, TestAck> {
        Scheduler::new(SchedulerConfig::default())
    }

    fn tracker(fx: &Fixture) -> FrontierTracker {
        FrontierTracker::new(PARTITION, None, fx.now)
    }

    fn subject() -> String {
        crate::topology::raw_subject(u64::from(PARTITION))
    }

    #[test]
    fn happy_path_queues_and_observes() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let bytes = fx.good().encode().unwrap();
        let subject = subject();

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 10),
            fx.now,
        );

        assert!(matches!(disposition, RawDisposition::Queued { depth: 1 }));
        assert!(!disposition.is_terminal());
        assert_eq!(sched.stats().pending, 1);
        assert_eq!(frontier.outstanding(), 1);
        assert_eq!(frontier.oldest_outstanding(), Some(10));
    }

    #[test]
    fn decoded_delivery_path_queues() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);

        let delivery = Delivery {
            item: fx.good(),
            handle: fx.ack(4),
            subject: subject(),
            msg_id: None,
            headers: HeaderMap::new(),
            sent_at: None,
            redelivered: false,
        };

        let disposition = reader.admit(&mut sched, &mut frontier, delivery, fx.now);
        assert!(matches!(disposition, RawDisposition::Queued { depth: 1 }));
        assert_eq!(sched.stats().pending, 1);
    }

    #[test]
    fn wrong_subject_partition_is_poison() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let bytes = fx.good().encode().unwrap();
        let subject = crate::topology::raw_subject(486);

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 1),
            fx.now,
        );

        match disposition {
            RawDisposition::Poison {
                handle,
                reason: PoisonReason::SubjectPartition { expected, got },
            } => {
                assert_eq!(handle.sequence(), 1);
                assert_eq!(expected, PARTITION);
                assert_eq!(got, Some(486));
            }
            other => panic!("expected SubjectPartition poison, got {other:?}"),
        }
        assert_eq!(frontier.outstanding(), 0);
        assert_eq!(sched.stats().pending, 0);
    }

    #[test]
    fn unparseable_subject_is_poison() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let bytes = fx.good().encode().unwrap();

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope("events.raw.p.not-a-number", None, &bytes, 1),
            fx.now,
        );
        assert!(matches!(
            disposition,
            RawDisposition::Poison {
                reason: PoisonReason::SubjectPartition { got: None, .. },
                ..
            }
        ));
    }

    #[test]
    fn undecodable_payload_is_poison() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let subject = subject();
        // A truncated length-delimited field: valid tag, length 127, no data.
        let bytes = [0x0a, 0x7f];

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 2),
            fx.now,
        );

        assert!(disposition.is_terminal());
        match disposition {
            RawDisposition::Poison {
                handle,
                reason: PoisonReason::Decode,
            } => assert_eq!(handle.sequence(), 2),
            other => panic!("expected Decode poison, got {other:?}"),
        }
    }

    #[test]
    fn wrong_vehicle_partition_is_poison() {
        let fx = Fixture::new();
        // Reader owns partition 0, but vehicle 1 hashes to 485.
        let reader = RawReader::new(0);
        let mut sched = scheduler();
        let mut frontier = FrontierTracker::new(0, None, fx.now);
        let bytes = fx.good().encode().unwrap();
        // Deliver it on partition 0's subject so the subject check passes first.
        let subject = crate::topology::raw_subject(0);

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 1),
            fx.now,
        );

        match disposition {
            RawDisposition::Poison {
                reason: PoisonReason::VehiclePartition { subject, vehicle },
                ..
            } => {
                assert_eq!(subject, 0);
                assert_eq!(vehicle, PARTITION);
            }
            other => panic!("expected VehiclePartition poison, got {other:?}"),
        }
    }

    #[test]
    fn schema_mismatch_is_poison() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let bytes = fx.good().encode().unwrap();
        let subject = subject();

        let mut wrong = HeaderMap::new();
        wrong.insert(headers::SCHEMA, (SCHEMA_VERSION.0 + 1).to_string());

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, Some(&wrong), &bytes, 1),
            fx.now,
        );
        match disposition {
            RawDisposition::Poison {
                reason: PoisonReason::Schema { got },
                ..
            } => assert_eq!(got, Some(SCHEMA_VERSION.0 + 1)),
            other => panic!("expected Schema poison, got {other:?}"),
        }
    }

    #[test]
    fn matching_schema_header_passes() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let bytes = fx.good().encode().unwrap();
        let subject = subject();

        let mut ok = HeaderMap::new();
        headers::stamp_schema(&mut ok);

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, Some(&ok), &bytes, 1),
            fx.now,
        );
        assert!(matches!(disposition, RawDisposition::Queued { .. }));
    }

    #[test]
    fn zero_vehicle_is_poison_invalid() {
        let fx = Fixture::new();
        // Vehicle 0 hashes to partition 0, so identity passes and sanity trips.
        let reader = RawReader::new(0);
        let mut sched = scheduler();
        let mut frontier = FrontierTracker::new(0, None, fx.now);
        let bytes = fx.payload(0, 151.0, -33.0).encode().unwrap();
        let subject = crate::topology::raw_subject(0);

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 1),
            fx.now,
        );
        match disposition {
            RawDisposition::Poison {
                reason: PoisonReason::Invalid { kind },
                ..
            } => assert_eq!(kind, "zero_vehicle"),
            other => panic!("expected Invalid poison, got {other:?}"),
        }
    }

    #[test]
    fn non_finite_coordinate_is_poison_invalid() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let bytes = fx.payload(VEHICLE, f64::NAN, -33.0).encode().unwrap();
        let subject = subject();

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 1),
            fx.now,
        );
        match disposition {
            RawDisposition::Poison {
                reason: PoisonReason::Invalid { kind },
                ..
            } => assert_eq!(kind, "non_finite"),
            other => panic!("expected Invalid poison, got {other:?}"),
        }
    }

    #[test]
    fn behind_frontier_is_suppressed_and_terminal() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        // Recover with the frontier already at 100.
        let mut frontier = FrontierTracker::new(
            PARTITION,
            Some(PartitionFrontier {
                partition: PARTITION,
                sequence: 100,
            }),
            fx.now,
        );
        let bytes = fx.good().encode().unwrap();
        let subject = subject();

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 50),
            fx.now,
        );
        assert!(disposition_terminal(&disposition));
        match disposition {
            RawDisposition::Suppressed {
                handle,
                reason: SuppressReason::BehindFrontier,
            } => assert_eq!(handle.sequence(), 50),
            other => panic!("expected BehindFrontier suppression, got {other:?}"),
        }
        assert_eq!(frontier.outstanding(), 0);
        assert_eq!(sched.stats().pending, 0);
    }

    #[test]
    fn duplicate_in_flight_is_coalesced_and_not_terminal() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let bytes = fx.good().encode().unwrap();
        let subject = subject();

        let first = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 7),
            fx.now,
        );
        assert!(matches!(first, RawDisposition::Queued { .. }));

        // A coalesced redelivery is not terminal: the in-flight original owns it.
        let again = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 7),
            fx.now,
        );
        match &again {
            RawDisposition::Suppressed {
                handle,
                reason: SuppressReason::Coalesced,
            } => assert_eq!(handle.sequence(), 7),
            other => panic!("expected Coalesced suppression, got {other:?}"),
        }
        assert!(!again.is_terminal());
        assert_eq!(sched.stats().pending, 1);
        assert_eq!(frontier.outstanding(), 1);
    }

    #[test]
    fn already_committed_is_suppressed_and_terminal() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let bytes = fx.good().encode().unwrap();
        let subject = subject();

        // Track at a low observation, then load a checkpoint committed to seq 100.
        let seed = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 10),
            fx.now,
        );
        assert!(matches!(seed, RawDisposition::Queued { .. }));
        sched.set_checkpoint(VehicleId(VEHICLE), fx.present_checkpoint(100));

        // Seq 50 is above the frontier but already covered by the checkpoint.
        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 50),
            fx.now,
        );
        match &disposition {
            RawDisposition::Suppressed {
                handle,
                reason: SuppressReason::Committed,
            } => assert_eq!(handle.sequence(), 50),
            other => panic!("expected Committed suppression, got {other:?}"),
        }
        assert!(disposition.is_terminal());
    }

    #[test]
    fn overflow_is_suppressed_and_terminal() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = Scheduler::<MockEntryId, TestAck>::new(SchedulerConfig {
            pending_limit: 1,
            ..SchedulerConfig::default()
        });
        let mut frontier = tracker(&fx);
        let bytes = fx.good().encode().unwrap();
        let subject = subject();

        let first = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 10),
            fx.now,
        );
        assert!(matches!(first, RawDisposition::Queued { depth: 1 }));

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 11),
            fx.now,
        );
        match &disposition {
            RawDisposition::Suppressed {
                handle,
                reason: SuppressReason::Overflow,
            } => assert_eq!(handle.sequence(), 11),
            other => panic!("expected Overflow suppression, got {other:?}"),
        }
        assert!(disposition.is_terminal());
    }

    #[test]
    fn returned_handle_can_be_acknowledged() {
        let fx = Fixture::new();
        let reader = RawReader::new(PARTITION);
        let mut sched = scheduler();
        let mut frontier = tracker(&fx);
        let bytes = fx.good().encode().unwrap();
        let subject = crate::topology::raw_subject(486); // wrong subject -> poison

        let disposition = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 9),
            fx.now,
        );
        let RawDisposition::Poison { handle, .. } = disposition else {
            panic!("expected poison");
        };
        futures::executor::block_on(handle.ack()).unwrap();
        assert_eq!(&*fx.log.lock().unwrap(), &[AckOp::Ack(9)]);
    }

    #[test]
    fn labels_are_bounded() {
        assert_eq!(PoisonReason::Decode.label(), "decode");
        assert_eq!(
            PoisonReason::SubjectPartition {
                expected: 0,
                got: None
            }
            .label(),
            "subject_partition"
        );
        assert_eq!(
            PoisonReason::VehiclePartition {
                subject: 0,
                vehicle: 1
            }
            .label(),
            "vehicle_partition"
        );
        assert_eq!(PoisonReason::Schema { got: None }.label(), "schema");
        assert_eq!(
            PoisonReason::Invalid {
                kind: "zero_vehicle"
            }
            .label(),
            "zero_vehicle"
        );
        assert_eq!(SuppressReason::BehindFrontier.label(), "behind_frontier");
        assert_eq!(SuppressReason::Committed.label(), "committed");
        assert_eq!(SuppressReason::Coalesced.label(), "coalesced");
        assert_eq!(SuppressReason::Overflow.label(), "overflow");
    }

    /// `is_terminal` on a borrowed disposition (the enum owns a non-`Copy`
    /// handle, so tests that keep asserting on it borrow through this).
    fn disposition_terminal(d: &RawDisposition<TestAck>) -> bool {
        d.is_terminal()
    }
}
