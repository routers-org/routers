//! Ordered raw reader: envelope validation and duplicate suppression. (T20)
//!
//! A partition worker owns one filtered view of the raw journal (spec §3): the
//! stream carries every partition's observations interleaved and the consumer
//! delivers only this partition's subject, in stream-sequence order. This
//! module is the thin, *pure* seam between that delivery and the scheduler: it
//! decides, for one raw message, whether it is a genuine new observation to
//! queue, a poison message to ack-and-forget, or a redelivery/already-decided
//! duplicate to suppress. It performs no I/O and does not acknowledge anything
//! itself — acknowledgement is the worker's move, made only once the frontier
//! has been told the message is done (see [`RawDisposition::is_terminal`]).
//!
//! # What "envelope validation" checks, and why here
//!
//! A raw message's *identity* is asserted in three independent places that must
//! agree, and a disagreement is a producer bug or a misrouted/spoofed message
//! that must never be solved under the wrong owner:
//!
//! 1. the **subject** it was delivered on (`events.raw.p.<p>`), read back with
//!    [`topology::partition_of_subject`];
//! 2. the **partition of the vehicle id** inside the payload
//!    ([`partition::partition_of`]) — the cross-language producer contract;
//! 3. the reader's own [`partition`](RawReader::partition).
//!
//! All three must be equal. On top of identity the reader enforces the wire
//! [`schema`](crate::protocol::ids::SCHEMA_VERSION) (when a peer stamped one)
//! and basic payload sanity, so a corrupt row never costs a solve.
//!
//! ## Why the reader does *not* re-apply the ingress age window
//!
//! [`ingress::validate`] also rejects observations outside an age/skew window,
//! but that is an *ingress admission* policy, not a reader invariant: a raw
//! message may legitimately sit in the journal for the whole retention window
//! before the orchestrator reads it, so re-applying the age window here would
//! poison correctly-journaled observations. The reader therefore reuses
//! [`ingress::validate`] for its *structural* checks (identity, finite and
//! in-range coordinates) with a deliberately unbounded time window
//! ([`SANITY_LIMITS`]), keeping one definition of "a sane payload" without
//! importing an admission decision that does not belong to it.
//!
//! # Suppression and the completion frontier
//!
//! Two frontier facts and the scheduler between them decide suppression:
//!
//! * [`FrontierTracker::observe`] reports a sequence at or below the frontier as
//!   [`Observed::BehindFrontier`] — already terminal, a bare redelivery — and a
//!   sequence still in flight as [`Observed::Duplicate`]. Only an
//!   [`Observed::New`] sequence reaches the scheduler.
//! * [`Scheduler::enqueue`] then coalesces a duplicate observation id, suppresses
//!   one already covered by the loaded checkpoint ([`Enqueue::Committed`]), or
//!   refuses one past the per-vehicle backlog ([`Enqueue::Overflow`]).
//!
//! The crucial asymmetry is *who completes the frontier*. A message the reader
//! suppresses because it is decided (behind the frontier, already committed, or
//! refused) is terminal: the worker acks it **and** completes its sequence so
//! the frontier can advance over it. A message suppressed because it *coalesced*
//! with work still in flight is **not** terminal: the in-flight original owns
//! that sequence's completion, and completing it here would advance the frontier
//! past an uncommitted message — exactly the "frontier must never lead commits"
//! violation the tracker exists to prevent. [`RawDisposition::is_terminal`]
//! encodes precisely this distinction.

use core::time::Duration;

use async_nats::HeaderMap;
use chrono::{DateTime, Utc};
use routers_network::Entry;
use tokio::time::Instant;

use crate::bus::Wire;
use crate::bus::adapter::{AckHandle, Delivery};
use crate::event::Payload;
use crate::ingress::{self, IngressLimits};
use crate::orchestrator::frontier::{FrontierTracker, Observed};
use crate::orchestrator::scheduler::{Enqueue, PendingObservation, Scheduler};
use crate::partition;
use crate::protocol::ids::{ObservationId, SCHEMA_VERSION, SchemaVersion, headers};
use crate::topology::partition_of_subject;

/// A time window wide enough that [`ingress::validate`] applies only its
/// structural checks (identity, finiteness, coordinate range) and never its
/// age/skew window. See the module docs for why the reader must not re-apply the
/// ingress age window.
const SANITY_LIMITS: IngressLimits = IngressLimits {
    max_age: Duration::from_secs(u64::MAX),
    max_ahead: Duration::from_secs(u64::MAX),
};

/// Why a raw message can never become an observation and is acked-and-forgotten.
///
/// Every variant is a permanent fault of *this* message: re-delivering it would
/// fail identically, so the worker acks it (retiring the delivery) and completes
/// its sequence rather than looping. The labels are bounded for metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoisonReason {
    /// The payload bytes did not decode as a protobuf [`Payload`].
    Decode,
    /// The delivery subject's partition token disagreed with the reader's
    /// partition (or was absent/malformed), so the message was misrouted.
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
    /// The payload failed a basic sanity check (a zero vehicle id, a non-finite
    /// or out-of-range coordinate). The label is the offending
    /// [`ingress::IngressError`] kind.
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
/// Unlike a [`PoisonReason`] the message was well-formed; it is simply already
/// decided or a duplicate. [`SuppressReason::Coalesced`] is the one reason that
/// is *not* terminal — see [`RawDisposition::is_terminal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuppressReason {
    /// The sequence is at or below the completion frontier — a redelivery of
    /// something already terminal.
    BehindFrontier,
    /// The loaded checkpoint already covers this observation's input.
    Committed,
    /// The same observation id is already pending or in flight; a duplicate
    /// transport delivery. The in-flight original owns its completion.
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
/// [`Queued`](RawDisposition::Queued) means the scheduler has *taken ownership*
/// of the observation (and its ack handle): the worker will acknowledge it only
/// after the work it drives commits, so no handle rides along here. The other
/// two variants hand the ack handle back, because the message will never occupy
/// a FIFO slot and the worker acks it straight away.
#[derive(Debug)]
#[must_use = "a disposition may carry an ack handle that must be acknowledged"]
pub enum RawDisposition<H: AckHandle> {
    /// The observation was appended to its vehicle's FIFO; `depth` is the new
    /// pending depth. The scheduler holds the handle.
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
    /// Whether the worker should mark this message's sequence *complete* on the
    /// frontier after acking it.
    ///
    /// A poison or already-decided message is terminal: nothing else will ever
    /// complete its sequence, so the frontier must advance over it. A
    /// [`SuppressReason::Coalesced`] message is **not** terminal — the still
    /// in-flight original owns that sequence's completion, and completing it
    /// here would let the frontier lead an uncommitted message. A
    /// [`Queued`](RawDisposition::Queued) message is not terminal either: its
    /// completion comes later, when its commit is promoted.
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
///
/// The worker cannot decode raw bytes inside a [`Source`](crate::bus::adapter::Source)
/// without losing the ack handle on a decode error (poison must still be
/// acked), so it hands the reader the undecoded envelope and lets
/// [`RawReader::admit_bytes`] own the decode.
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
/// It holds only the partition it owns; all the mutable state it drives — the
/// scheduler and the frontier tracker — is passed in per call, so the reader is
/// trivially shareable and every decision is a pure function of its inputs.
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

    /// Classify an already-decoded delivery (the transport decoded the
    /// [`Payload`] at the seam).
    ///
    /// A [`Delivery`] carries no headers, so no schema header is available to
    /// check here; a decoded delivery has already proven wire compatibility, and
    /// the schema check is treated as "absent header" (which is a pass, since
    /// the header is optional). Use [`RawReader::admit_bytes`] on the raw path
    /// where the header is present and a decode may fail.
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
    ///
    /// The subject is validated before the decode (a misrouted message is not
    /// worth decoding), then the payload is decoded, then the shared envelope
    /// and suppression checks run.
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

    /// The shared tail: vehicle-partition and schema identity, payload sanity,
    /// then the frontier and scheduler suppression steps. The subject has
    /// already been validated by the caller.
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
        // Identity: the vehicle id must hash to the partition it arrived on.
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

        // Schema: optional, but a stamped mismatch is poison.
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

        // Sanity: structural checks only (see `SANITY_LIMITS`).
        if let Err(error) = ingress::validate(&payload, sanity_reference(), &SANITY_LIMITS) {
            return RawDisposition::Poison {
                handle,
                reason: PoisonReason::Invalid { kind: error.kind() },
            };
        }

        // Frontier: a redelivery of something terminal or still in flight never
        // reaches the scheduler.
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

        // Scheduler: it takes ownership of a queued observation (and its handle);
        // otherwise it hands the handle back with the reason it declined.
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

/// The fixed reference instant [`ingress::validate`] is called against. With
/// [`SANITY_LIMITS`]'s unbounded window the reference never affects the outcome,
/// so a constant epoch keeps the sanity check deterministic and clock-free.
fn sanity_reference() -> DateTime<Utc> {
    DateTime::from_timestamp(0, 0).expect("the unix epoch is a valid timestamp")
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use std::sync::Mutex;

    use chrono::TimeZone;
    use geo::Point;
    use routers_network::mock::MockEntryId;

    use super::*;
    use crate::event::VehicleId;
    use crate::orchestrator::scheduler::{CheckpointState, SchedulerConfig};
    use crate::protocol::ids::{GraphVersion, RegionId, Revision, SCHEMA_VERSION, SegmentId};
    use crate::store::checkpoint::{PartitionFrontier, VehicleCheckpoint};

    use routers_transition::matcher::Trip;

    /// A recorded acknowledgement.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum AckOp {
        Ack(u64),
        Nak(u64),
    }

    /// A minimal [`AckHandle`] that records its ack/nak so a test can prove the
    /// right handle came back and could be retired.
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

    /// Vehicle 1 hashes to partition 485 (pinned in `partition.rs`); the tests
    /// use that pairing so subject, vehicle, and reader agree.
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

        /// A well-formed payload for the pinned vehicle.
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

    /// The canonical subject for the pinned partition.
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
        // The scheduler owns the observation and its handle.
        assert_eq!(sched.stats().pending, 1);
        // The frontier is now tracking the sequence, held just below it.
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
        // A subject for a different partition than the reader owns.
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
        // A bad subject never reaches the frontier or scheduler.
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

        // A redelivery at sequence 50 is below the frontier.
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
        // It was never tracked, so nothing became outstanding.
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

        // First delivery queues and observes.
        let first = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 7),
            fx.now,
        );
        assert!(matches!(first, RawDisposition::Queued { .. }));

        // A redelivery of the same sequence coalesces; the in-flight original
        // owns its completion, so this is *not* terminal.
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
        // Still exactly one queued observation and one outstanding sequence.
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

        // Track the vehicle with a low observation (keeping the frontier low),
        // then load a checkpoint whose last committed input is sequence 100.
        let seed = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 10),
            fx.now,
        );
        assert!(matches!(seed, RawDisposition::Queued { .. }));
        sched.set_checkpoint(VehicleId(VEHICLE), fx.present_checkpoint(100));

        // A fresh delivery at sequence 50 is new to the frontier (above it) but
        // already covered by the checkpoint's last committed input.
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

        // Fill the single pending slot.
        let first = reader.admit_bytes(
            &mut sched,
            &mut frontier,
            fx.envelope(&subject, None, &bytes, 10),
            fx.now,
        );
        assert!(matches!(first, RawDisposition::Queued { depth: 1 }));

        // A different sequence for the same vehicle overflows the FIFO.
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
