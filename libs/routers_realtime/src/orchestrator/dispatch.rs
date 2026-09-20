//! Job builder and publisher: the orchestrator's dispatch step.
//!
//! Builds a deterministic, immutable solve context from a checkpoint and head
//! observation, reserves admission credit before publishing, then republishes the
//! identical bytes under the job's [`JobId`] on an ambiguous outcome so the broker
//! dedups. A pure builder with no mutable state: the vehicle is never advanced on
//! a failed publish, and the scheduler is left to the caller.

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

/// The ceiling the doubling publish backoff never exceeds.
const BACKOFF_CAP: Duration = Duration::from_secs(2);

/// Static configuration for a [`Dispatcher`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DispatchConfig {
    /// How many publish attempts an ambiguous outcome may consume before dispatch gives up.
    pub attempts: u32,
    /// The initial backoff between ambiguous-publish retries; doubles each retry up to a 2 s ceiling.
    pub backoff: Duration,
    /// The largest time gap between the last committed observation and a new one that still counts as the same journey; a larger gap forces a segment reset.
    pub gap: Duration,
    /// The largest straight-line jump (metres, haversine) that is not treated as a teleport.
    pub jump_distance_m: f64,
}

impl Default for DispatchConfig {
    /// The spec defaults: five attempts, 100 ms backoff, 120 s gap, 2 km teleport threshold.
    fn default() -> Self {
        Self {
            attempts: 5,
            backoff: Duration::from_millis(100),
            gap: Duration::from_secs(120),
            jump_distance_m: 2_000.0,
        }
    }
}

/// The immutable solve context [`build_context`] derives for one observation.
pub struct Built<E: Entry> {
    /// The resume/restart state the matcher solves from.
    pub continuation: Continuation<E>,
    /// The committed state a commit compare-and-swaps against, or `None` for a fresh vehicle with no checkpoint yet.
    pub base: Option<BaseState>,
    /// The continuity segment this job belongs to.
    pub segment: SegmentId,
    /// Why continuity was broken; `None` means the job continues the existing segment.
    pub reset: Option<ResetReason>,
}

/// The [`Origin`] a payload represents: position and supplier timestamp in unix microseconds.
fn origin_of(payload: &Payload) -> Origin {
    Origin::new(payload.point, payload.timestamp.timestamp_micros())
}

/// Whether the step from the last committed origin to `head` breaks continuity, and why.
fn continuity_break(last: &Origin, head: &Origin, cfg: &DispatchConfig) -> Option<ResetReason> {
    // A time gap is checked before a spatial jump, so a stale-then-teleported vehicle reports the gap.
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
/// Pure and clock-free: identical inputs always yield an identical context (and
/// job id), which is what lets an ambiguous publish be retried byte-for-byte.
pub fn build_context<E: Entry>(
    checkpoint: Option<&VehicleCheckpoint<E>>,
    observation: ObservationId,
    head: &Payload,
    cfg: &DispatchConfig,
) -> Built<E> {
    let head_origin = origin_of(head);
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

    // The commit CASes on this revision even on a reset, so a racing commit is still caught.
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

/// Why a [`Dispatcher::dispatch`] did not publish a job. Every variant leaves the
/// vehicle's state untouched and releases any reserved permit.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// A credit scope was at capacity; nothing was published and the caller retries later.
    #[error("admission held: {0}")]
    Held(HeldReason),
    /// The resolved region is not among those the admission controller was built for.
    #[error("region is not known to the admission controller")]
    UnknownRegion,
    /// The job could not be encoded.
    #[error("could not encode the solve job: {0}")]
    Encode(anyhow::Error),
    /// The publish failed outright, or stayed ambiguous past the retry budget; the permit was released.
    #[error("job publish failed: {0}")]
    Publish(#[source] PublishError),
}

/// A successfully dispatched job: the [`ActiveJob`] to attach to the vehicle (it
/// owns the admission [`Permit`]) plus the provenance a commit later needs.
#[derive(Debug)]
pub struct Dispatched {
    /// The single logical job now in flight for the vehicle.
    pub job: ActiveJob,
    /// Why continuity was broken for this job, if it was.
    pub reset: Option<ResetReason>,
    /// The continuity segment this job belongs to.
    pub segment: SegmentId,
    /// `true` when the broker recognised the publish as a duplicate that had already landed.
    pub duplicate: bool,
}

/// Builds and publishes solve jobs over a [`Publisher`]. Stateless, so one
/// instance is shared by a process's partition workers without coordination.
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
    /// `now` is the monotonic reference for the returned [`ActiveJob`]'s deadline;
    /// `now_us` is the absolute unix-microsecond deadline the matcher uses. On
    /// [`DispatchError::Held`] nothing was published or reserved; on the other
    /// errors the permit is released and the vehicle state is left ready to
    /// re-dispatch the identical job.
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

        let bytes = job.encode().map_err(DispatchError::Encode)?;
        let byte_len = bytes.len() as u64;

        // Reserve credit before anything reaches a matcher queue.
        let permit: Permit = match admission.try_admit(&resolution.region, byte_len) {
            Ok(permit) => permit,
            Err(AdmitError::Held(held)) => return Err(DispatchError::Held(held.reason)),
            Err(AdmitError::UnknownRegion) => return Err(DispatchError::UnknownRegion),
        };

        let subject = job_subject(&resolution.graph, &resolution.region, resolution.lane);
        let msg_id = job.msg_id();

        // On any error return, `permit` drops here and releases its credit.
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
                Err(PublishError::Failed(cause)) => {
                    return Err(DispatchError::Publish(PublishError::Failed(cause)));
                }
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

    fn continuing_head() -> Payload {
        let last = *trace_origins().last().unwrap();
        payload(
            1,
            point!(x: -118.180, y: 34.1403),
            last.timestamp + 5_000_000,
        )
    }

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
        assert_eq!(built.segment, SegmentId(999));
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
        assert_eq!(adm.global().jobs, 1);

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
        assert_eq!(adm.global().jobs, 0);
        assert_eq!(adm.snapshot()[0].jobs, 0);
    }
}
