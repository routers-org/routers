//! Job builder and publisher: the orchestrator's dispatch step. (spec §3, §10)
//!
//! Dispatch is the moment a vehicle's next observation becomes a
//! [`SolveJob`] on the wire. It does three things, in order, and nothing else:
//!
//! 1. **Builds a deterministic, immutable context.** [`build_context`] turns the
//!    vehicle's committed checkpoint plus its head observation into a
//!    [`Continuation`] (resume the retained trip, or restart it) and the
//!    provenance a commit needs — the [`BaseState`] to compare-and-swap against
//!    and the continuity [`SegmentId`]. Because the context is a pure function
//!    of the checkpoint and the head, two orchestrators (or the same one
//!    retrying) that see the same inputs mint byte-identical job bytes and the
//!    same [`JobId`].
//! 2. **Reserves admission before publishing.** A job's encoded size is charged
//!    against the [`Admission`] controller *first*; only once a [`Permit`] is
//!    held does the job reach a matcher queue. Work that cannot be admitted is
//!    left waiting outside the solve plane (spec §3) — dispatch publishes
//!    nothing and hands the reason back so the caller can record held demand.
//! 3. **Publishes with acknowledgement, retrying an ambiguous outcome.** The
//!    same bytes are (re)published under the job's [`JobId`] as `Nats-Msg-Id`. A
//!    [`PublishError::Ambiguous`] outcome — the publish may or may not have
//!    landed — is retried with those identical bytes so the broker's dedup
//!    window collapses a double landing (spec §10, "job publish outcome
//!    unknown"). The vehicle's state is never advanced on a failed publish; the
//!    [`Permit`] is dropped, its credits released, and the caller re-dispatches
//!    the identical job later.
//!
//! Dispatch does not touch the scheduler. On success it hands the caller a fully
//! formed [`ActiveJob`] (permit included) plus the reset/segment provenance; the
//! worker is what calls `scheduler.activate` and arms the deadline. Keeping that
//! split means this module has no mutable state and stays a pure builder wrapped
//! around one publish.

use core::time::Duration;

use geo::{Distance, Haversine};
use routers_network::Entry;
use routers_transition::matcher::{Continuation, Origin};
use thiserror::Error;
use tokio::time::{Instant, sleep};

use crate::bus::Wire;
use crate::bus::adapter::{AckHandle, PublishError, PublishOutcome, Publisher};
use crate::bus::outbound;
use crate::event::{Payload, VehicleId};
use crate::orchestrator::admission::{Admission, AdmitError, HeldReason, Permit};
use crate::orchestrator::scheduler::{ActiveJob, PendingObservation};
use crate::protocol::ids::headers::stamp_schema;
use crate::protocol::ids::{ObservationId, SCHEMA_VERSION, SegmentId};
use crate::protocol::job::{BaseState, JobIdentity, SolveJob};
use crate::protocol::output::ResetReason;
use crate::region::resolver::Resolution;
use crate::store::checkpoint::VehicleCheckpoint;
use crate::topology::jobs::job_subject;

/// The ceiling the doubling publish backoff never exceeds. Kept as a constant
/// rather than a [`DispatchConfig`] field so the config stays the four knobs the
/// shared contract fixes; the cap is a property of the retry loop, not a tuning
/// dial callers vary.
const BACKOFF_CAP: Duration = Duration::from_secs(2);

/// Static configuration for a [`Dispatcher`].
///
/// The two publish knobs bound how hard an ambiguous publish is retried; the two
/// continuity knobs are the thresholds [`build_context`] uses to decide a job
/// must restart a vehicle's segment rather than resume it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DispatchConfig {
    /// How many publish attempts an ambiguous outcome may consume before dispatch
    /// gives up (and the caller re-dispatches the identical job later).
    pub attempts: u32,
    /// The initial backoff between ambiguous-publish retries. It doubles each
    /// retry up to an internal 2-second ceiling.
    pub backoff: Duration,
    /// The largest gap between the last committed observation and a new one that
    /// still counts as the same journey; a larger gap forces a segment reset.
    pub gap: Duration,
    /// The largest straight-line jump (metres, haversine) between the last
    /// committed observation and a new one that is not treated as a teleport.
    pub jump_distance_m: f64,
}

impl Default for DispatchConfig {
    /// The spec defaults: five publish attempts, a 100 ms initial backoff, a
    /// 120-second continuity gap, and a 2 km teleport threshold.
    fn default() -> Self {
        Self {
            attempts: 5,
            backoff: Duration::from_millis(100),
            gap: Duration::from_secs(120),
            jump_distance_m: 2_000.0,
        }
    }
}

/// The immutable solve context [`build_context`] derives for one observation:
/// what the matcher should resume from, what a commit must compare against, and
/// whether continuity was broken.
pub struct Built<E: Entry> {
    /// The resume/restart state the matcher solves from.
    pub continuation: Continuation<E>,
    /// The committed state a commit compare-and-swaps against, or `None` for a
    /// fresh vehicle with no checkpoint yet. Present even on a reset, because the
    /// commit still races the *current* revision.
    pub base: Option<BaseState>,
    /// The continuity segment this job belongs to — the checkpoint's segment when
    /// resuming, a fresh segment (the head observation's sequence) on a reset or
    /// a brand-new vehicle.
    pub segment: SegmentId,
    /// Why continuity was broken, if it was. `None` means the job continues the
    /// existing segment.
    pub reset: Option<ResetReason>,
}

/// The [`Origin`] a payload represents: its position and its supplier timestamp
/// in unix microseconds (the same units the wire and the matcher use).
fn origin_of(payload: &Payload) -> Origin {
    Origin::new(payload.point, payload.timestamp.timestamp_micros())
}

/// Whether the step from the last committed origin to `head` breaks continuity,
/// and why. A gap in time is checked before a jump in space, so a stale-then-
/// -teleported vehicle reports the gap (either way its segment restarts).
fn continuity_break(last: &Origin, head: &Origin, cfg: &DispatchConfig) -> Option<ResetReason> {
    // Saturating so a (suppressed upstream, `debug_assert`ed below) timestamp
    // regression cannot overflow; a non-positive elapsed never trips the gap.
    let elapsed_us = head.timestamp.saturating_sub(last.timestamp);
    if elapsed_us > cfg.gap.as_micros() as i64 {
        return Some(ResetReason::Gap);
    }
    if Haversine.distance(last.point, head.point) > cfg.jump_distance_m {
        return Some(ResetReason::Teleport);
    }
    None
}

/// Build the deterministic, immutable context for solving `head` (identified by
/// `observation`) given the vehicle's committed `checkpoint`.
///
/// The rules (spec §3, §8):
///
/// * **No checkpoint** (a fresh vehicle): [`Continuation::Restart`] from the head
///   alone, no [`BaseState`], a segment valued as the head observation's
///   sequence, and no reset.
/// * **Checkpoint present, continuity broken** — the head is more than
///   [`DispatchConfig::gap`] after, or more than [`DispatchConfig::jump_distance_m`]
///   from, the last committed origin: restart from the head into a *new* segment,
///   carrying the current [`BaseState`] (the commit still compare-and-swaps on
///   the current revision) and the [`ResetReason`].
/// * **Checkpoint present, continuous**: hand the retained trip and the committed
///   history (its origins plus the head) to [`Continuation::reconcile`], which
///   decides resume-versus-restart, keep the checkpoint's segment, and carry the
///   current [`BaseState`].
///
/// Pure and clock-free: the same inputs always yield the same context (and hence
/// the same [`JobId`]), which is what lets an ambiguous publish be retried
/// byte-for-byte.
pub fn build_context<E: Entry>(
    checkpoint: Option<&VehicleCheckpoint<E>>,
    observation: ObservationId,
    head: &Payload,
    cfg: &DispatchConfig,
) -> Built<E> {
    let head_origin = origin_of(head);
    // A fresh segment is always valued as the opening observation's sequence, so
    // segments are deterministic without a shared counter.
    let fresh_segment = SegmentId::from(observation);

    let Some(checkpoint) = checkpoint else {
        return Built {
            continuation: Continuation::Restart {
                fresh: vec![head_origin],
            },
            base: None,
            segment: fresh_segment,
            reset: None,
        };
    };

    // The commit compares against exactly this revision/segment, on a reset or
    // not — a racing commit must still be caught.
    let current = BaseState {
        revision: checkpoint.revision,
        segment: checkpoint.segment,
    };

    let last = checkpoint.trip.origins().last();
    debug_assert!(
        last.is_none_or(|last| head_origin.timestamp > last.timestamp),
        "head observation must be newer than the last committed origin \
         (the reader/scheduler suppress regressions)"
    );

    if let Some(reason) = last.and_then(|last| continuity_break(last, &head_origin, cfg)) {
        return Built {
            continuation: Continuation::Restart {
                fresh: vec![head_origin],
            },
            base: Some(current),
            segment: fresh_segment,
            reset: Some(reason),
        };
    }

    // Continuous: let the library reconcile the retained trip with the committed
    // history extended by the head, and keep the same segment.
    let mut history: Vec<Origin> = checkpoint.trip.origins().to_vec();
    history.push(head_origin);
    let continuation = Continuation::reconcile(Some(checkpoint.trip.clone()), &history);

    Built {
        continuation,
        base: Some(current),
        segment: checkpoint.segment,
        reset: None,
    }
}

/// Why a [`Dispatcher::dispatch`] did not publish a job and record an active one.
///
/// Every variant leaves the vehicle's state untouched: on [`DispatchError::Held`]
/// no permit was reserved, and on the publish/encode variants the permit is
/// dropped (its credits released) before the error returns.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// A credit scope was at capacity, so the vehicle is left waiting outside the
    /// matcher queues. The caller records held demand and retries later; nothing
    /// was published.
    #[error("admission held: {0}")]
    Held(HeldReason),
    /// The resolved region was not among those the admission controller was built
    /// for — a configuration mismatch, not backpressure.
    #[error("region is not known to the admission controller")]
    UnknownRegion,
    /// The job could not be encoded. Unreachable for an in-memory value (postcard
    /// of these types is infallible), but surfaced rather than panicked.
    #[error("could not encode the solve job: {0}")]
    Encode(anyhow::Error),
    /// The publish failed outright, or stayed ambiguous past the retry budget. The
    /// permit was released; the caller re-dispatches the identical job later.
    #[error("job publish failed: {0}")]
    Publish(#[source] PublishError),
}

/// A successfully dispatched job: the [`ActiveJob`] to attach to the vehicle (it
/// owns the admission [`Permit`]) plus the provenance a commit later needs.
///
/// The caller records `job` with `scheduler.activate`, arms its deadline, and
/// carries `reset`/`segment` into the commit so a broken segment emits its
/// [`OutputKind::Reset`](crate::protocol::output::OutputKind::Reset) before the
/// match.
#[derive(Debug)]
pub struct Dispatched {
    /// The single logical job now in flight for the vehicle.
    pub job: ActiveJob,
    /// Why continuity was broken for this job, if it was.
    pub reset: Option<ResetReason>,
    /// The continuity segment this job belongs to.
    pub segment: SegmentId,
    /// `true` when the broker recognised the publish as a duplicate — the job was
    /// already stored (an ambiguous publish that had in fact landed).
    pub duplicate: bool,
}

/// Builds and publishes solve jobs over a [`Publisher`].
///
/// Stateless apart from its `publisher` and `cfg`: every call to
/// [`dispatch`](Self::dispatch) is self-contained, so one `Dispatcher` is shared
/// by a process's partition workers without coordination.
pub struct Dispatcher<P> {
    publisher: P,
    cfg: DispatchConfig,
}

impl<P> Dispatcher<P> {
    /// Wrap a `publisher` and configuration.
    pub fn new(publisher: P, cfg: DispatchConfig) -> Self {
        Self { publisher, cfg }
    }

    /// The dispatcher's configuration.
    pub fn config(&self) -> &DispatchConfig {
        &self.cfg
    }

    /// Build, admit, and publish the solve job for `head`.
    ///
    /// `now` is the monotonic reference the returned [`ActiveJob`] deadlines and
    /// dispatch time are measured from (the worker's local scheduling clock);
    /// `now_us` is the absolute unix-microsecond wall clock the *matcher*
    /// deadlines the job against. They are separate because the two consumers
    /// keep separate clocks — the local deadline heap is monotonic, the wire
    /// deadline is absolute.
    ///
    /// Returns [`Dispatched`] on success. On [`DispatchError::Held`] nothing was
    /// published and no credit reserved; on the publish/encode errors the permit
    /// is released and the vehicle state is left exactly as it was, ready to
    /// re-dispatch the identical job (spec §10).
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch<E, H>(
        &self,
        vehicle: VehicleId,
        head: &PendingObservation<H>,
        checkpoint: Option<&VehicleCheckpoint<E>>,
        resolution: &Resolution,
        admission: &Admission,
        now: Instant,
        now_us: i64,
    ) -> Result<Dispatched, DispatchError>
    where
        E: Entry,
        SolveJob<E>: Wire,
        P: Publisher<SolveJob<E>>,
        H: AckHandle,
    {
        debug_assert_eq!(
            vehicle, head.payload.vehicle_id,
            "the dispatched vehicle must match the head observation's payload"
        );

        let Built {
            continuation,
            base,
            segment,
            reset,
        } = build_context(checkpoint, head.id, &head.payload, &self.cfg);

        let identity = JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: vehicle,
            observation: head.id,
            base,
            graph: resolution.graph.clone(),
            region: resolution.region.clone(),
        };
        let deadline_us = now_us + resolution.budget.as_micros() as i64;
        let job = SolveJob::new(identity.clone(), resolution.lane, deadline_us, continuation);

        // Encode once: the same bytes are what admission charges for, what the
        // broker dedups on, and what every retry republishes.
        let bytes = job.encode().map_err(DispatchError::Encode)?;
        let byte_len = bytes.len() as u64;

        // Reserve credit before anything reaches a matcher queue. A held request
        // publishes nothing; the caller keeps the vehicle waiting.
        let permit: Permit = match admission.try_admit(&resolution.region, byte_len) {
            Ok(permit) => permit,
            Err(AdmitError::Held(held)) => return Err(DispatchError::Held(held.reason)),
            Err(AdmitError::UnknownRegion) => return Err(DispatchError::UnknownRegion),
        };

        let subject = job_subject(&resolution.graph, &resolution.region, resolution.lane);
        let msg_id = job.msg_id();

        // Publish with acknowledgement. On any error return, `permit` drops here
        // and releases its credit — the vehicle state stays as it was.
        let duplicate = self
            .publish_bytes_with_retry(&subject, &msg_id, &bytes)
            .await?;

        Ok(Dispatched {
            job: ActiveJob {
                id: job.id,
                identity,
                observation: head.id,
                deadline: now + resolution.budget,
                bytes: byte_len,
                permit,
                dispatched: now,
            },
            reset,
            segment,
            duplicate,
        })
    }

    /// Publish `bytes` under `subject`/`msg_id`, retrying an ambiguous outcome
    /// with the identical bytes and a doubling backoff. Returns whether the
    /// broker deduped the (eventually accepted) publish.
    async fn publish_bytes_with_retry<E>(
        &self,
        subject: &str,
        msg_id: &str,
        bytes: &[u8],
    ) -> Result<bool, DispatchError>
    where
        E: Entry,
        SolveJob<E>: Wire,
        P: Publisher<SolveJob<E>>,
    {
        let mut delay = self.cfg.backoff;
        let mut attempt: u32 = 1;
        loop {
            // Fresh headers each attempt: a new send time, this build's schema,
            // and the current trace context. The dedup key stays `msg_id`.
            let mut headers = outbound();
            stamp_schema(&mut headers);

            match Publisher::<SolveJob<E>>::publish_bytes(
                &self.publisher,
                subject,
                msg_id,
                headers,
                bytes,
            )
            .await
            {
                Ok(PublishOutcome::Acked { duplicate, .. }) => return Ok(duplicate),
                // Known failure: nothing landed, so surface it and release credit.
                Err(PublishError::Failed(cause)) => {
                    return Err(DispatchError::Publish(PublishError::Failed(cause)));
                }
                // Unknown outcome: a copy may already be stored. Retry the exact
                // bytes so the broker dedups, until the attempt budget is spent.
                Err(PublishError::Ambiguous(cause)) => {
                    if attempt >= self.cfg.attempts {
                        return Err(DispatchError::Publish(PublishError::Ambiguous(cause)));
                    }
                    sleep(delay).await;
                    delay = delay.saturating_mul(2).min(BACKOFF_CAP);
                    attempt += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use geo::{Point, point};
    use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
    use routers_transition::Matcher;
    use routers_transition::costing::{
        CostingStrategies, DefaultEmissionCost, DefaultTransitionCost,
    };
    use routers_transition::layer::generation::StandardGenerator;
    use routers_transition::matcher::Trip;
    use routers_transition::weigh::AllCompute;

    use super::*;
    use crate::bus::memory::MemoryBus;
    use crate::orchestrator::admission::{Admission, AdmissionConfig};
    use crate::protocol::ids::{GraphVersion, Lane, RegionId, Revision};
    use crate::region::resolver::ResolutionKind;

    type Costing = CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, MockEntryId>;

    /// A trivial ack handle: dispatch never touches it, so it records nothing.
    struct NoAck;

    impl AckHandle for NoAck {
        async fn ack(self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn nak(self, _delay: Option<Duration>) -> anyhow::Result<()> {
            Ok(())
        }
        fn sequence(&self) -> u64 {
            0
        }
        fn deliveries(&self) -> u32 {
            1
        }
    }

    fn graph() -> GraphVersion {
        GraphVersion::new("g1").unwrap()
    }

    fn region() -> RegionId {
        RegionId::new("r1").unwrap()
    }

    /// The budget every fixture resolution grants; deadlines derive from it.
    const BUDGET: Duration = Duration::from_millis(30_000);

    fn resolution() -> Resolution {
        Resolution {
            region: region(),
            graph: graph(),
            lane: Lane(2),
            budget: BUDGET,
            kind: ResolutionKind::Repinned,
            routing_version: 1,
        }
    }

    /// An admission controller that can hold `jobs` outstanding jobs in the
    /// fixture region (a generous byte budget so only the job cap ever bites).
    fn admission(jobs: u64) -> Admission {
        let cfg = AdmissionConfig {
            region_jobs: jobs,
            ..AdmissionConfig::default()
        };
        Admission::new(cfg, core::iter::once(&region()))
    }

    fn payload(vehicle: u64, pt: Point, ts_us: i64) -> Payload {
        Payload {
            vehicle_id: VehicleId(vehicle),
            timestamp: DateTime::<Utc>::from_timestamp_micros(ts_us).unwrap(),
            point: pt,
        }
    }

    fn pending(vehicle: u64, seq: u64, pt: Point, ts_us: i64) -> PendingObservation<NoAck> {
        PendingObservation {
            id: ObservationId {
                partition: 7,
                sequence: seq,
            },
            payload: payload(vehicle, pt, ts_us),
            handle: NoAck,
            received: Instant::now(),
        }
    }

    /// A staircase road (same shape the matched-diff test uses) that the
    /// observation trace below anchors cleanly against.
    fn bent_road() -> MockNetwork {
        MockNetworkBuilder::new()
            .node(1, point!(x: -118.15, y: 34.15))
            .node(2, point!(x: -118.16, y: 34.15))
            .node(3, point!(x: -118.17, y: 34.15))
            .node(4, point!(x: -118.17, y: 34.14))
            .node(5, point!(x: -118.18, y: 34.14))
            .edge(1, 2)
            .edge(2, 3)
            .edge(3, 4)
            .edge(4, 5)
            .build()
    }

    /// The first supplier stamp of the trace; observations march forward from it.
    const TRACE_START_US: i64 = 1_775_000_000_000_000;

    fn trace_origins() -> Vec<Origin> {
        [
            point!(x: -118.151, y: 34.1503),
            point!(x: -118.155, y: 34.1503),
            point!(x: -118.165, y: 34.1503),
            point!(x: -118.170, y: 34.1490),
            point!(x: -118.172, y: 34.1403),
            point!(x: -118.179, y: 34.1403),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, pt)| Origin::new(pt, TRACE_START_US + i as i64 * 5_000_000))
        .collect()
    }

    /// Build a resumable [`Trip`] by pushing `origins` through a matcher on the
    /// staircase network (the only way to mint a non-empty trip).
    fn trip_with(origins: &[Origin]) -> Trip<MockEntryId> {
        let net = bent_road();
        let costing = Costing::default();
        let generator = StandardGenerator::new(&net, &costing.emission);
        let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());
        let mut trip = m.begin();
        for &origin in origins {
            m.push(&mut trip, origin).expect("observation must anchor");
        }
        trip
    }

    /// A committed checkpoint whose retained trip is the whole `trace_origins`
    /// trace, at the given revision and segment.
    fn checkpoint(revision: u64, segment: u64) -> VehicleCheckpoint<MockEntryId> {
        VehicleCheckpoint {
            trip: trip_with(&trace_origins()),
            last_input: ObservationId {
                partition: 7,
                sequence: revision,
            },
            revision: Revision(revision),
            segment: SegmentId(segment),
            finalized_through: None,
            graph: graph(),
            schema: SCHEMA_VERSION,
            region: region(),
        }
    }

    /// The timestamp just after the last trace origin, and its position — the
    /// natural continuation of the trace.
    fn continuing_head() -> Payload {
        let last = *trace_origins().last().unwrap();
        payload(
            1,
            point!(x: -118.180, y: 34.1403),
            last.timestamp + 5_000_000,
        )
    }

    // ----- build_context (pure) -----

    #[test]
    fn fresh_vehicle_restarts_with_no_base() {
        let cfg = DispatchConfig::default();
        let obs = ObservationId {
            partition: 7,
            sequence: 42,
        };
        let head = payload(1, point!(x: -118.15, y: 34.15), TRACE_START_US);

        let built = build_context::<MockEntryId>(None, obs, &head, &cfg);

        assert!(built.base.is_none());
        assert!(built.reset.is_none());
        assert_eq!(built.segment, SegmentId(42));
        match built.continuation {
            Continuation::Restart { fresh } => assert_eq!(fresh, vec![origin_of(&head)]),
            Continuation::Resume { .. } => panic!("a fresh vehicle must Restart"),
        }
    }

    #[test]
    fn continuous_checkpoint_resumes_with_the_head_fresh() {
        let cfg = DispatchConfig::default();
        let cp = checkpoint(500, 7);
        let head = continuing_head();
        let obs = ObservationId {
            partition: 7,
            sequence: 999,
        };

        let built = build_context(Some(&cp), obs, &head, &cfg);

        assert!(built.reset.is_none());
        // Same segment as the checkpoint (continuity preserved), CASed on it.
        assert_eq!(built.segment, SegmentId(7));
        assert_eq!(
            built.base,
            Some(BaseState {
                revision: Revision(500),
                segment: SegmentId(7),
            })
        );
        match built.continuation {
            Continuation::Resume { fresh, .. } => assert_eq!(fresh, vec![origin_of(&head)]),
            Continuation::Restart { .. } => panic!("a continuous checkpoint must Resume"),
        }
    }

    #[test]
    fn a_time_gap_restarts_a_new_segment() {
        let cfg = DispatchConfig::default();
        let cp = checkpoint(500, 7);
        let last = *trace_origins().last().unwrap();
        // Just over the 120 s gap, but in the same place: a gap, not a teleport.
        let head = payload(
            1,
            point!(x: -118.179, y: 34.1403),
            last.timestamp + cfg.gap.as_micros() as i64 + 1_000_000,
        );
        let obs = ObservationId {
            partition: 7,
            sequence: 999,
        };

        let built = build_context(Some(&cp), obs, &head, &cfg);

        assert_eq!(built.reset, Some(ResetReason::Gap));
        assert_eq!(built.segment, SegmentId(999)); // a fresh segment
        assert_eq!(
            built.base,
            Some(BaseState {
                revision: Revision(500),
                segment: SegmentId(7),
            }),
            "a reset still CASes on the current revision"
        );
        assert!(matches!(built.continuation, Continuation::Restart { .. }));
    }

    #[test]
    fn a_spatial_jump_restarts_as_a_teleport() {
        let cfg = DispatchConfig::default();
        let cp = checkpoint(500, 7);
        let last = *trace_origins().last().unwrap();
        // Within the time gap, but ~9 km away: a teleport.
        let head = payload(
            1,
            point!(x: -118.079, y: 34.1403),
            last.timestamp + 5_000_000,
        );
        let obs = ObservationId {
            partition: 7,
            sequence: 999,
        };

        let built = build_context(Some(&cp), obs, &head, &cfg);

        assert_eq!(built.reset, Some(ResetReason::Teleport));
        assert_eq!(built.segment, SegmentId(999));
        assert!(matches!(built.continuation, Continuation::Restart { .. }));
    }

    // ----- dispatch (async, over MemoryBus) -----

    fn dispatcher(
        bus: &MemoryBus,
        cfg: DispatchConfig,
    ) -> Dispatcher<impl Publisher<SolveJob<MockEntryId>>> {
        Dispatcher::new(bus.publisher::<SolveJob<MockEntryId>>(), cfg)
    }

    #[tokio::test]
    async fn fresh_dispatch_publishes_a_verifiable_job() {
        let bus = MemoryBus::new();
        let dispatcher = dispatcher(&bus, DispatchConfig::default());
        let adm = admission(10);
        let res = resolution();
        let head = pending(1, 42, point!(x: -118.15, y: 34.15), TRACE_START_US);
        let now = Instant::now();
        let now_us = TRACE_START_US;

        let dispatched = dispatcher
            .dispatch::<MockEntryId, _>(VehicleId(1), &head, None, &res, &adm, now, now_us)
            .await
            .expect("dispatch");

        assert!(dispatched.job.identity.base.is_none());
        assert_eq!(dispatched.segment, SegmentId(42));
        assert!(dispatched.reset.is_none());
        assert!(!dispatched.duplicate);
        assert_eq!(dispatched.job.observation, head.id);
        assert_eq!(dispatched.job.id, dispatched.job.identity.job_id());
        // The permit is retained in the active job.
        assert_eq!(adm.global().jobs, 1);

        // Exactly one message, addressed by the job id, that verifies and decodes.
        let subject = job_subject(&res.graph, &res.region, res.lane);
        let published = bus.published(&subject);
        assert_eq!(published.len(), 1);
        let (subj, msg_id, bytes) = &published[0];
        assert_eq!(subj, &subject);
        assert_eq!(
            msg_id.as_deref(),
            Some(dispatched.job.id.to_string().as_str())
        );
        assert_eq!(dispatched.job.bytes as usize, bytes.len());

        let decoded = SolveJob::<MockEntryId>::decode_verified(bytes).expect("decode + verify");
        assert_eq!(decoded.id, dispatched.job.id);
        assert_eq!(decoded.lane, Lane(2));
        assert_eq!(decoded.deadline_us, now_us + BUDGET.as_micros() as i64);
        assert_eq!(decoded.head(), Some(&origin_of(&head.payload)));
        assert!(matches!(decoded.context, Continuation::Restart { .. }));
    }

    #[tokio::test]
    async fn continuation_dispatch_resumes_from_the_checkpoint() {
        let bus = MemoryBus::new();
        let dispatcher = dispatcher(&bus, DispatchConfig::default());
        let adm = admission(10);
        let res = resolution();
        let cp = checkpoint(500, 7);
        let head_payload = continuing_head();
        let head = PendingObservation {
            id: ObservationId {
                partition: 7,
                sequence: 999,
            },
            payload: head_payload,
            handle: NoAck,
            received: Instant::now(),
        };

        let dispatched = dispatcher
            .dispatch::<MockEntryId, _>(
                VehicleId(1),
                &head,
                Some(&cp),
                &res,
                &adm,
                Instant::now(),
                TRACE_START_US,
            )
            .await
            .expect("dispatch");

        assert!(dispatched.reset.is_none());
        assert_eq!(dispatched.segment, SegmentId(7));
        assert_eq!(
            dispatched.job.identity.base,
            Some(BaseState {
                revision: Revision(500),
                segment: SegmentId(7),
            })
        );

        let subject = job_subject(&res.graph, &res.region, res.lane);
        let published = bus.published(&subject);
        assert_eq!(published.len(), 1);
        let decoded = SolveJob::<MockEntryId>::decode_verified(&published[0].2).expect("decode");
        assert!(matches!(decoded.context, Continuation::Resume { .. }));
        assert_eq!(decoded.head(), Some(&origin_of(&head.payload)));
    }

    #[tokio::test]
    async fn ambiguous_publish_retries_the_same_bytes_and_dedups() {
        let bus = MemoryBus::new();
        // Zero backoff so the single retry does not stall the test.
        let cfg = DispatchConfig {
            backoff: Duration::ZERO,
            ..DispatchConfig::default()
        };
        let dispatcher = dispatcher(&bus, cfg);
        let adm = admission(10);
        let res = resolution();
        let head = pending(1, 42, point!(x: -118.15, y: 34.15), TRACE_START_US);

        // The first publish lands but its ack is lost; the retry must dedup.
        bus.fail_next_publish(PublishError::Ambiguous(anyhow::anyhow!("timeout")));

        let dispatched = dispatcher
            .dispatch::<MockEntryId, _>(
                VehicleId(1),
                &head,
                None,
                &res,
                &adm,
                Instant::now(),
                TRACE_START_US,
            )
            .await
            .expect("dispatch");

        assert!(dispatched.duplicate, "the retry must observe the dedup");
        let subject = job_subject(&res.graph, &res.region, res.lane);
        assert_eq!(bus.published(&subject).len(), 1, "only one copy is stored");
        assert_eq!(adm.global().jobs, 1, "the permit is retained on success");
    }

    #[tokio::test]
    async fn held_admission_publishes_and_reserves_nothing() {
        let bus = MemoryBus::new();
        let dispatcher = dispatcher(&bus, DispatchConfig::default());
        // Zero-job region: every admission is held.
        let adm = admission(0);
        let res = resolution();
        let head = pending(1, 42, point!(x: -118.15, y: 34.15), TRACE_START_US);

        let err = dispatcher
            .dispatch::<MockEntryId, _>(
                VehicleId(1),
                &head,
                None,
                &res,
                &adm,
                Instant::now(),
                TRACE_START_US,
            )
            .await
            .expect_err("held");

        assert!(matches!(err, DispatchError::Held(HeldReason::RegionJobs)));
        let subject = job_subject(&res.graph, &res.region, res.lane);
        assert!(bus.published(&subject).is_empty(), "nothing is published");
        assert_eq!(adm.global().jobs, 0, "no credit is reserved");
        assert_eq!(adm.snapshot()[0].jobs, 0);
    }

    #[tokio::test]
    async fn failed_publish_releases_the_permit() {
        let bus = MemoryBus::new();
        let dispatcher = dispatcher(&bus, DispatchConfig::default());
        let adm = admission(10);
        let res = resolution();
        let head = pending(1, 42, point!(x: -118.15, y: 34.15), TRACE_START_US);

        bus.fail_next_publish(PublishError::Failed(anyhow::anyhow!("refused")));

        let err = dispatcher
            .dispatch::<MockEntryId, _>(
                VehicleId(1),
                &head,
                None,
                &res,
                &adm,
                Instant::now(),
                TRACE_START_US,
            )
            .await
            .expect_err("failed");

        assert!(matches!(
            err,
            DispatchError::Publish(PublishError::Failed(_))
        ));
        let subject = job_subject(&res.graph, &res.region, res.lane);
        assert!(
            bus.published(&subject).is_empty(),
            "a failed publish stores nothing"
        );
        // The permit dropped on the error return, so no credit leaked.
        assert_eq!(adm.global().jobs, 0);
        assert_eq!(adm.snapshot()[0].jobs, 0);
    }
}
