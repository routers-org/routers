//! Job builder and publisher: the orchestrator's dispatch step.
//!
//! Builds an immutable solve context from a checkpoint and raw observation,
//! reserves admission credit, and sends a transient request. The worker
//! retains the exact full request bytes for retries until a result arrives.

use async_nats::HeaderValue;
use core::str::FromStr;
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
use crate::orchestrator::scheduler::{ActiveJob, JobReservation, PendingObservation};
use crate::protocol::ids::headers::stamp_schema;
use crate::protocol::ids::{ObservationId, SCHEMA_VERSION, SegmentId};
use crate::protocol::job::{BaseState, JobIdentity, SolveJob, SolveRequest, TripDigest};
use crate::protocol::output::ResetReason;
use crate::region::resolver::Resolution;
use crate::store::checkpoint::VehicleCheckpoint;
use crate::topology::{reply_subject, request_subject};

/// Header carrying the process-scoped reply subject for a transient solve request.
pub const REPLY_HEADER: &str = "X-Solve-Reply";

/// Header on a solve result naming the answering matcher's direct request
/// subject, where that matcher holds the trip it just produced.
pub const STICKY_HEADER: &str = "X-Solve-Matcher";

/// A request retained only while its durable raw observation is uncommitted.
/// A retry always resends the full continuation to the queue group.
#[derive(Clone, Debug)]
pub struct RetryRequest<E: Entry> {
    pub subject: String,
    pub msg_id: String,
    pub reply: String,
    pub full: FullRequest<E>,
}

/// The full request a retry sends: already encoded when it was the first
/// send, or encoded on first use after a trip-less send to a sticky replica,
/// which usually succeeds without ever needing it.
#[derive(Clone, Debug)]
pub enum FullRequest<E: Entry> {
    /// The exact bytes already sent.
    Encoded(Vec<u8>),
    /// The job to encode if a retry is needed.
    Deferred(Box<SolveJob<E>>),
}

impl<E: Entry> RetryRequest<E> {
    /// The full request's bytes, encoding a deferred job once.
    pub fn encoded(&mut self) -> Result<&[u8], DispatchError>
    where
        SolveRequest<E>: Wire,
    {
        if let FullRequest::Deferred(job) = &self.full {
            let bytes = SolveRequest::full(job)
                .encode()
                .map_err(DispatchError::Encode)?;
            self.full = FullRequest::Encoded(bytes);
        }
        match &self.full {
            FullRequest::Encoded(bytes) => Ok(bytes),
            FullRequest::Deferred(_) => unreachable!("encoded above"),
        }
    }
}

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
    /// The digest of a resume's trip, taken from the sealed checkpoint when
    /// the resume carries its trip unchanged. `None` for a restart.
    pub trip_digest: Option<TripDigest>,
}

/// A same-vehicle temporal ordering violation that must be terminally committed
/// instead of reaching the matcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("observation timestamp {incoming} is not newer than committed timestamp {committed}")]
pub struct TimestampRegression {
    /// The last origin retained by the committed vehicle checkpoint.
    pub committed: i64,
    /// The timestamp carried by the newer raw stream sequence.
    pub incoming: i64,
}

/// Classify a newer raw sequence whose supplier time does not advance the same
/// vehicle's committed origin.
#[must_use]
pub fn timestamp_regression<E: Entry>(
    checkpoint: Option<&VehicleCheckpoint<E>>,
    observation: ObservationId,
    head: &Payload,
) -> Option<TimestampRegression> {
    let checkpoint = checkpoint?;
    if observation.sequence <= checkpoint.last_input.sequence {
        return None;
    }
    let committed = checkpoint.trip.origins().last()?.timestamp;
    let incoming = head.timestamp.timestamp_micros();
    (incoming <= committed).then_some(TimestampRegression {
        committed,
        incoming,
    })
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
) -> Result<Built<E>, TimestampRegression> {
    let head_origin = origin_of(head);
    let fresh_segment = SegmentId::from(observation);

    let Some(checkpoint) = checkpoint else {
        return Ok(Built {
            continuation: Continuation::Restart {
                fresh: vec![head_origin],
            },
            base: None,
            segment: fresh_segment,
            reset: None,
            trip_digest: None,
        });
    };

    // The commit CASes on this revision even on a reset, so a racing commit is still caught.
    let current = BaseState {
        revision: checkpoint.revision,
        segment: checkpoint.segment,
    };

    if let Some(regression) = timestamp_regression(Some(checkpoint), observation, head) {
        return Err(regression);
    }
    let last = checkpoint.trip.origins().last();

    if let Some(reason) = last.and_then(|last| continuity_break(last, &head_origin, cfg)) {
        return Ok(Built {
            continuation: Continuation::Restart {
                fresh: vec![head_origin],
            },
            base: Some(current),
            segment: fresh_segment,
            reset: Some(reason),
            trip_digest: None,
        });
    }

    let mut history: Vec<Origin> = checkpoint.trip.origins().to_vec();
    history.push(head_origin);
    let continuation = Continuation::reconcile(Some(checkpoint.trip.clone()), &history);
    // `reconcile` only ever shortens the trip, so an unchanged layer count
    // means the resume carries exactly the sealed trip.
    let trip_digest = match &continuation {
        Continuation::Resume { trip, .. } => Some(
            checkpoint
                .trip_digest
                .filter(|_| trip.layers() == checkpoint.trip.layers())
                .unwrap_or_else(|| TripDigest::of(trip)),
        ),
        Continuation::Restart { .. } => None,
    };

    Ok(Built {
        continuation,
        base: Some(current),
        segment: checkpoint.segment,
        reset: None,
        trip_digest,
    })
}

/// Why a [`Dispatcher::dispatch`] did not publish a job. Every variant leaves the
/// vehicle's state untouched and releases any reserved permit.
#[derive(Debug, Error)]
pub enum DispatchError {
    /// A newer raw sequence did not advance the vehicle's committed event time.
    #[error(transparent)]
    TimestampRegression(#[from] TimestampRegression),
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
pub struct Dispatched<E: Entry> {
    /// The single logical job now in flight for the vehicle.
    pub job: ActiveJob,
    /// Why continuity was broken for this job, if it was.
    pub reset: Option<ResetReason>,
    /// The continuity segment this job belongs to.
    pub segment: SegmentId,
    /// `true` when the broker recognised the publish as a duplicate that had already landed.
    pub duplicate: bool,
    /// Time spent deriving and encoding the deterministic job.
    pub built_for: Duration,
    /// Time spent publishing the encoded job and awaiting the broker.
    pub published_for: Duration,
    /// Retained bytes for a retry: always the full continuation on the
    /// queue-group subject, so a retry never depends on the sticky replica.
    pub request: RetryRequest<E>,
    /// `true` when the first send went to a sticky replica without the trip.
    pub cached: bool,
}

/// Builds and sends solve requests over a [`Publisher`]. Stateless, so one
/// instance is shared by a process's partition workers without coordination.
pub struct Dispatcher<P> {
    publisher: P,
    cfg: DispatchConfig,
    reply_prefix: String,
}

impl<P> Dispatcher<P> {
    /// Wrap a `publisher` and configuration. Answers are addressed to
    /// `reply_prefix.p.<partition>`, this process's inbox.
    pub fn new(publisher: P, cfg: DispatchConfig, reply_prefix: String) -> Self {
        Self {
            publisher,
            cfg,
            reply_prefix,
        }
    }

    /// The dispatcher's configuration.
    pub fn config(&self) -> &DispatchConfig {
        &self.cfg
    }

    /// Build, admit, and publish the solve job for `head`.
    ///
    /// `now_us` is the absolute unix-microsecond freshness target the matcher
    /// observes. On
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
    ) -> Result<Dispatched<E>, DispatchError>
    where
        E: Entry,
        SolveRequest<E>: Wire,
        P: Publisher<SolveRequest<E>>,
        H: AckHandle,
    {
        self.dispatch_to(
            vehicle, head, checkpoint, resolution, admission, now, now_us, None,
        )
        .await
    }

    /// As [`dispatch`](Self::dispatch), but on the transient plane a resume is
    /// sent without its trip to `sticky`: the replica that produced the
    /// checkpoint's trip and advertised it. The caller must only name a replica
    /// whose trip is for exactly `checkpoint`'s revision; the job id guards the
    /// rest, and a miss or timeout falls back to the retained full request.
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_to<E, H>(
        &self,
        vehicle: VehicleId,
        head: &PendingObservation<H>,
        checkpoint: Option<&VehicleCheckpoint<E>>,
        resolution: &Resolution,
        admission: &Admission,
        now: Instant,
        _now_us: i64,
        sticky: Option<&str>,
    ) -> Result<Dispatched<E>, DispatchError>
    where
        E: Entry,
        SolveRequest<E>: Wire,
        P: Publisher<SolveRequest<E>>,
        H: AckHandle,
    {
        debug_assert_eq!(
            vehicle, head.payload.vehicle_id,
            "the dispatched vehicle must match the head observation's payload"
        );

        let build_started = Instant::now();
        let Built {
            continuation,
            base,
            segment,
            reset,
            trip_digest,
        } = build_context(checkpoint, head.id, &head.payload, &self.cfg)?;

        let identity = JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: vehicle,
            observation: head.id,
            base,
            graph: resolution.graph.clone(),
            region: resolution.region.clone(),
        };
        let freshness_target_us = head.published_at.freshness_target_after(resolution.budget);
        let job = SolveJob::with_trip_digest(
            identity.clone(),
            resolution.lane,
            freshness_target_us,
            continuation,
            trip_digest,
        );

        // A resume steered to its sticky replica is first sent without the
        // trip, and its full form is only encoded if a retry needs it.
        // Anything else is sent in full, and those exact bytes are retained.
        let cached_send = match sticky.zip(SolveRequest::cached(&job)) {
            Some((subject, request)) => Some((
                subject.to_owned(),
                request.encode().map_err(DispatchError::Encode)?,
            )),
            None => None,
        };
        let full = match &cached_send {
            Some(_) => None,
            None => Some(
                SolveRequest::full(&job)
                    .encode()
                    .map_err(DispatchError::Encode)?,
            ),
        };
        let byte_len = match (&cached_send, &full) {
            (Some((_, cached)), _) => cached.len(),
            (None, Some(full)) => full.len(),
            (None, None) => unreachable!("one form is always encoded"),
        } as u64;
        let built_for = build_started.elapsed();

        // Reserve credit before anything reaches a matcher queue.
        let permit: Permit = match admission.try_admit(&resolution.region, byte_len) {
            Ok(permit) => permit,
            Err(AdmitError::Held(held)) => return Err(DispatchError::Held(held.reason)),
            Err(AdmitError::UnknownRegion) => return Err(DispatchError::UnknownRegion),
        };

        let subject = request_subject(&resolution.graph, &resolution.region, resolution.lane);
        let msg_id = job.msg_id();
        let reply = reply_subject(&self.reply_prefix, head.id.partition);

        // On any error return, `permit` drops here and releases its credit.
        let publish_started = Instant::now();
        let duplicate = match (&cached_send, &full) {
            (Some((sticky, cached)), _) => {
                self.publish_bytes_with_retry(sticky, &msg_id, &reply, cached)
                    .await?
            }
            (None, Some(full)) => {
                self.publish_bytes_with_retry(&subject, &msg_id, &reply, full)
                    .await?
            }
            (None, None) => unreachable!("one form is always encoded"),
        };
        let published_for = publish_started.elapsed();

        Ok(Dispatched {
            job: ActiveJob {
                id: job.id,
                identity,
                observation: head.id,
                bytes: byte_len,
                reservation: JobReservation::Admitted(permit),
                dispatched: now,
            },
            reset,
            segment,
            duplicate,
            built_for,
            published_for,
            cached: cached_send.is_some(),
            request: RetryRequest {
                subject,
                msg_id,
                reply,
                full: match full {
                    Some(bytes) => FullRequest::Encoded(bytes),
                    None => FullRequest::Deferred(Box::new(job)),
                },
            },
        })
    }

    /// Resend the full request after a lost core message, a trip miss, or a
    /// matcher failure, encoding a deferred one on first use.
    pub async fn republish<E>(&self, request: &mut RetryRequest<E>) -> Result<(), DispatchError>
    where
        E: Entry,
        SolveRequest<E>: Wire,
        P: Publisher<SolveRequest<E>>,
    {
        let subject = request.subject.clone();
        let msg_id = request.msg_id.clone();
        let reply = request.reply.clone();
        let bytes = request.encoded()?;
        self.publish_bytes_with_retry::<E>(&subject, &msg_id, &reply, bytes)
            .await?;
        Ok(())
    }

    /// Publish `bytes` under `subject`/`msg_id`, retrying an ambiguous outcome
    /// with the identical bytes and a doubling backoff. Returns whether the
    /// broker deduped the (eventually accepted) publish.
    async fn publish_bytes_with_retry<E>(
        &self,
        subject: &str,
        msg_id: &str,
        reply: &str,
        bytes: &[u8],
    ) -> Result<bool, DispatchError>
    where
        E: Entry,
        SolveRequest<E>: Wire,
        P: Publisher<SolveRequest<E>>,
    {
        let mut delay = self.cfg.backoff;
        let mut attempt: u32 = 1;
        loop {
            let mut headers = outbound();
            stamp_schema(&mut headers);
            headers.insert(
                REPLY_HEADER,
                HeaderValue::from_str(reply)
                    .map_err(|error| DispatchError::Publish(PublishError::Failed(error.into())))?,
            );

            match Publisher::<SolveRequest<E>>::publish_bytes(
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
    use crate::orchestrator::scheduler::PublishedAtMicros;
    use crate::protocol::ids::{GraphVersion, Lane, RegionId, Revision};
    use crate::protocol::job::{ResolveError, TripDigest};
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
            published_at: PublishedAtMicros::from_unix_micros(ts_us)
                .expect("test timestamp is non-negative"),
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
            routing_version: 1,
            trip_digest: None,
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

        let built = build_context::<MockEntryId>(None, obs, &head, &cfg).unwrap();

        assert!(built.base.is_none());
        assert!(built.reset.is_none());
        assert_eq!(built.segment, SegmentId(42));
        match built.continuation {
            Continuation::Restart { fresh } => assert_eq!(fresh, vec![origin_of(&head)]),
            Continuation::Resume { .. } => panic!("a fresh vehicle must Restart"),
        }
    }

    #[test]
    fn a_resume_reuses_the_sealed_trip_digest() {
        let cfg = DispatchConfig::default();
        let head = continuing_head();
        let obs = ObservationId {
            partition: 7,
            sequence: 999,
        };

        // An unsealed checkpoint digests its trip once.
        let cp = checkpoint(500, 7);
        let built = build_context(Some(&cp), obs, &head, &cfg).unwrap();
        assert_eq!(built.trip_digest, Some(TripDigest::of(&cp.trip)));

        // A sealed one is trusted as is: a marker digest proves it is reused.
        let mut sealed = checkpoint(500, 7);
        sealed.trip_digest = Some(TripDigest(7));
        let built = build_context(Some(&sealed), obs, &head, &cfg).unwrap();
        assert_eq!(built.trip_digest, Some(TripDigest(7)));

        // A continuity break restarts and authenticates no trip.
        let far = payload(1, point!(x: -100.0, y: 10.0), TRACE_START_US + 60_000_000);
        let built = build_context(Some(&sealed), obs, &far, &cfg).unwrap();
        assert!(built.trip_digest.is_none());
    }

    #[test]
    fn a_held_trip_digest_yields_the_same_job_as_redigesting() {
        let cp = checkpoint(500, 7);
        let head = continuing_head();
        let obs = ObservationId {
            partition: 7,
            sequence: 999,
        };
        let built = build_context(Some(&cp), obs, &head, &DispatchConfig::default()).unwrap();
        let identity = JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(1),
            observation: obs,
            base: built.base,
            graph: graph(),
            region: region(),
        };
        let held = SolveJob::with_trip_digest(
            identity.clone(),
            Lane::DEFAULT,
            10,
            built.continuation.clone(),
            built.trip_digest,
        );
        let redigested = SolveJob::new(identity, Lane::DEFAULT, 10, built.continuation);
        assert_eq!(held.id, redigested.id);
        held.verify().expect("the held digest matches the trip");
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

        let built = build_context(Some(&cp), obs, &head, &cfg).unwrap();

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

        let built = build_context(Some(&cp), obs, &head, &cfg).unwrap();

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

        let built = build_context(Some(&cp), obs, &head, &cfg).unwrap();

        assert_eq!(built.reset, Some(ResetReason::Teleport));
        assert_eq!(built.segment, SegmentId(999));
        assert!(matches!(built.continuation, Continuation::Restart { .. }));
    }

    #[test]
    fn newer_sequence_with_non_increasing_timestamp_is_a_typed_regression() {
        let cfg = DispatchConfig::default();
        let cp = checkpoint(500, 7);
        let last = trace_origins().last().unwrap().timestamp;

        for incoming in [last - 1, last] {
            let head = payload(1, point!(x: -118.180, y: 34.1403), incoming);
            let obs = ObservationId {
                partition: 7,
                sequence: 501,
            };

            match build_context(Some(&cp), obs, &head, &cfg) {
                Err(regression) => assert_eq!(
                    regression,
                    TimestampRegression {
                        committed: last,
                        incoming,
                    }
                ),
                Ok(_) => panic!("non-increasing timestamp must not build solve context"),
            }
        }
    }

    fn dispatcher(
        bus: &MemoryBus,
        cfg: DispatchConfig,
    ) -> Dispatcher<impl Publisher<SolveRequest<MockEntryId>>> {
        Dispatcher::new(
            bus.publisher::<SolveRequest<MockEntryId>>(),
            cfg,
            "_INBOX.test".to_owned(),
        )
    }

    /// Decode a sent full request back into its verified job.
    fn decode_request(bytes: &[u8]) -> Result<SolveJob<MockEntryId>, ResolveError> {
        SolveRequest::<MockEntryId>::decode(bytes)
            .expect("request decodes")
            .resolve(|_, _| None)
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
        assert_eq!(adm.global().jobs, 1);

        let subject = request_subject(&res.graph, &res.region, res.lane);
        let published = bus.published(&subject);
        assert_eq!(published.len(), 1);
        let (subj, msg_id, bytes) = &published[0];
        assert_eq!(subj, &subject);
        assert_eq!(
            msg_id.as_deref(),
            Some(dispatched.job.id.to_string().as_str())
        );
        assert_eq!(dispatched.job.bytes as usize, bytes.len());

        let decoded = decode_request(bytes).expect("decode + verify");
        assert_eq!(decoded.id, dispatched.job.id);
        assert_eq!(decoded.lane, Lane(2));
        assert_eq!(
            decoded.freshness_target_us,
            now_us + BUDGET.as_micros() as i64
        );
        assert_eq!(decoded.head(), Some(&origin_of(&head.payload)));
        assert!(matches!(decoded.context, Continuation::Restart { .. }));
    }

    #[tokio::test]
    async fn transient_dispatch_retains_exact_bytes_for_retry() {
        type Publications = Vec<(String, String, Vec<u8>)>;
        #[derive(Clone, Default)]
        struct Capture(alloc::sync::Arc<std::sync::Mutex<Publications>>);
        impl Publisher<SolveRequest<MockEntryId>> for Capture {
            async fn publish_bytes(
                &self,
                subject: &str,
                msg_id: &str,
                _headers: async_nats::HeaderMap,
                bytes: &[u8],
            ) -> Result<PublishOutcome, PublishError> {
                self.0.lock().unwrap().push((
                    subject.to_owned(),
                    msg_id.to_owned(),
                    bytes.to_vec(),
                ));
                Ok(PublishOutcome::Acked {
                    sequence: 0,
                    duplicate: false,
                })
            }
        }
        let capture = Capture::default();
        let dispatcher = Dispatcher::new(
            capture.clone(),
            DispatchConfig::default(),
            "_INBOX.test".to_owned(),
        );
        let head = pending(1, 42, point!(x: -118.15, y: 34.15), TRACE_START_US);
        let dispatched = dispatcher
            .dispatch::<MockEntryId, _>(
                VehicleId(1),
                &head,
                None,
                &resolution(),
                &admission(10),
                Instant::now(),
                TRACE_START_US,
            )
            .await
            .expect("dispatch");
        let mut request = dispatched.request;
        assert!(
            matches!(request.full, FullRequest::Encoded(_)),
            "a full first send retains its exact bytes"
        );
        assert!(request.subject.starts_with("solve.req.v1."));
        assert_eq!(
            request.reply,
            format!("_INBOX.test.p.{}", head.id.partition)
        );
        dispatcher
            .republish::<MockEntryId>(&mut request)
            .await
            .expect("retry");
        let published = capture.0.lock().unwrap();
        assert_eq!(published.len(), 2);
        assert_eq!(published[0].2, published[1].2);
        assert_eq!(published[0].1, published[1].1);
        assert_eq!(published[0].2, request.encoded().unwrap());
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
            published_at: PublishedAtMicros::from_unix_micros(TRACE_START_US)
                .expect("test timestamp is non-negative"),
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

        let subject = request_subject(&res.graph, &res.region, res.lane);
        let published = bus.published(&subject);
        assert_eq!(published.len(), 1);
        let decoded = decode_request(&published[0].2).expect("decode");
        assert!(matches!(decoded.context, Continuation::Resume { .. }));
        assert_eq!(decoded.head(), Some(&origin_of(&head.payload)));
    }

    #[tokio::test]
    async fn sticky_dispatch_omits_the_trip_and_retains_the_full_request() {
        let bus = MemoryBus::new();
        let dispatcher = dispatcher(&bus, DispatchConfig::default());
        let res = resolution();
        let cp = checkpoint(500, 7);
        let head = PendingObservation {
            id: ObservationId {
                partition: 7,
                sequence: 999,
            },
            payload: continuing_head(),
            published_at: PublishedAtMicros::from_unix_micros(TRACE_START_US)
                .expect("test timestamp is non-negative"),
            handle: NoAck,
            received: Instant::now(),
        };
        let sticky = "solve.req.v1.g.g1.r.r1.m.replica";

        let dispatched = dispatcher
            .dispatch_to::<MockEntryId, _>(
                VehicleId(1),
                &head,
                Some(&cp),
                &res,
                &admission(10),
                Instant::now(),
                TRACE_START_US,
                Some(sticky),
            )
            .await
            .expect("dispatch");
        assert!(dispatched.cached);

        let sent = bus.published(sticky);
        assert_eq!(sent.len(), 1, "the first send goes to the sticky replica");
        assert!(
            bus.published(&request_subject(&res.graph, &res.region, res.lane))
                .is_empty(),
            "nothing reached the queue group yet"
        );
        let mut request = dispatched.request;
        assert!(
            matches!(request.full, FullRequest::Deferred(_)),
            "the full form waits until a retry needs it"
        );
        assert_eq!(
            request.subject,
            request_subject(&res.graph, &res.region, res.lane),
            "a retry always goes to the queue group"
        );
        let full_bytes = request.encoded().unwrap().to_vec();
        assert!(
            sent[0].2.len() < full_bytes.len(),
            "the cached send is smaller"
        );

        // The retained retry is the full continuation; the cached send resolves
        // to the same verified job once the replica supplies the checkpoint trip.
        let full = SolveRequest::<MockEntryId>::decode(&full_bytes)
            .unwrap()
            .resolve(|_, _| None)
            .expect("the full request stands alone");
        let cached = SolveRequest::<MockEntryId>::decode(&sent[0].2)
            .unwrap()
            .resolve(|vehicle, revision| {
                assert_eq!((vehicle, revision), (VehicleId(1), Revision(500)));
                Some((cp.trip.clone(), TripDigest::of(&cp.trip)))
            })
            .expect("the cached trip reproduces the job id");
        assert_eq!(full.id, dispatched.job.id);
        assert_eq!(cached.id, dispatched.job.id);
    }

    #[tokio::test]
    async fn sticky_dispatch_sends_a_restart_in_full() {
        let bus = MemoryBus::new();
        let dispatcher = dispatcher(&bus, DispatchConfig::default());
        let res = resolution();
        let head = pending(1, 42, point!(x: -118.15, y: 34.15), TRACE_START_US);
        let sticky = "solve.req.v1.g.g1.r.r1.m.replica";
        let dispatched = dispatcher
            .dispatch_to::<MockEntryId, _>(
                VehicleId(1),
                &head,
                None,
                &res,
                &admission(10),
                Instant::now(),
                TRACE_START_US,
                Some(sticky),
            )
            .await
            .expect("dispatch");
        assert!(!dispatched.cached, "a fresh vehicle has no trip to omit");
        assert!(bus.published(sticky).is_empty());
        assert_eq!(
            bus.published(&request_subject(&res.graph, &res.region, res.lane))
                .len(),
            1
        );
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
        let subject = request_subject(&res.graph, &res.region, res.lane);
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
        let subject = request_subject(&res.graph, &res.region, res.lane);
        assert!(bus.published(&subject).is_empty(), "nothing is published");
        assert_eq!(adm.global().jobs, 0, "no credit is reserved");
        assert_eq!(adm.snapshot()[0].jobs, 0);
    }

    #[tokio::test]
    async fn stamped_raw_rebuilds_the_same_job_id_and_freshness_target_after_redelivery() {
        let bus = MemoryBus::new();
        let dispatcher = dispatcher(&bus, DispatchConfig::default());
        let adm = admission(10);
        let res = resolution();
        let mut head = pending(1, 42, point!(x: -118.15, y: 34.15), TRACE_START_US);
        let published_us = 1_775_000_000_000_000i64;
        head.published_at = PublishedAtMicros::from_unix_micros(published_us)
            .expect("test timestamp is non-negative");

        let first = dispatcher
            .dispatch::<MockEntryId, _>(
                VehicleId(1),
                &head,
                None,
                &res,
                &adm,
                Instant::now(),
                published_us + 1_000,
            )
            .await
            .expect("first dispatch");
        let first_id = first.job.id;
        drop(first);

        let replay = dispatcher
            .dispatch::<MockEntryId, _>(
                VehicleId(1),
                &head,
                None,
                &res,
                &adm,
                Instant::now(),
                published_us + 9_000_000,
            )
            .await
            .expect("redelivery dispatch");

        assert_eq!(replay.job.id, first_id);
        assert!(replay.duplicate, "stable proof deduplicates at the broker");
        let subject = request_subject(&res.graph, &res.region, res.lane);
        let records = bus.published(&subject);
        assert_eq!(records.len(), 1);
        let job = decode_request(&records[0].2).expect("job decodes");
        assert_eq!(
            job.freshness_target_us,
            published_us + i64::try_from(BUDGET.as_micros()).unwrap()
        );
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
        let subject = request_subject(&res.graph, &res.region, res.lane);
        assert!(
            bus.published(&subject).is_empty(),
            "a failed publish stores nothing"
        );
        assert_eq!(adm.global().jobs, 0);
        assert_eq!(adm.snapshot()[0].jobs, 0);
    }
}
