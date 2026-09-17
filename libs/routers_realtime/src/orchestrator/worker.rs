//! Orchestrator: the single-task partition worker loop.
//!
//! One worker owns one partition's state and `select!`s over raw deliveries,
//! result deliveries, deadline ticks, and a housekeeping tick. Invariants: one
//! active job per vehicle, per-vehicle FIFO on the raw sequence, and commits run
//! inline on this task so they never overlap. The `bus::adapter` futures are not
//! `Send`, so every source, publisher, and ack stays on this one task.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::time::Duration;
use std::collections::{HashMap, HashSet};

use routers_network::Entry;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};
use tracing::{debug, error, warn};
use web_time::UNIX_EPOCH;

use crate::bus::adapter::{AckHandle, Delivery, Publisher, Source};
use crate::event::VehicleId;
use crate::lifecycle::{Drain, Shutdown};
use crate::matcher::pull::RawBytes;
use crate::metrics::Metrics;
use crate::orchestrator::admission::{Admission, Waiting};
use crate::orchestrator::commit::{self, CommitConfig, CommitError, Committer, Decision};
use crate::orchestrator::deadline::{self, DeadlineConfig, Deadlines, Expiry};
use crate::orchestrator::dispatch::{DispatchConfig, DispatchError, Dispatcher};
use crate::orchestrator::frontier::{FrontierConfig, FrontierTracker};
use crate::orchestrator::reader::{RawDisposition, RawEnvelope, RawReader, SuppressReason};
use crate::orchestrator::recovery::{RecoveryReport, restore_vehicle};
use crate::orchestrator::scheduler::{
    ActiveJob, CheckpointState, Scheduler, SchedulerConfig, VehicleState,
};
use crate::orchestrator::validate::{self, QuarantineReason, RejectReason, Verdict};
use crate::protocol::ids::{GraphVersion, RegionId, Revision, SCHEMA_VERSION, SegmentId};
use crate::protocol::job::{BaseState, JobIdentity, SolveJob};
use crate::protocol::output::{CommittedOutput, ResetReason, TerminalReason};
use crate::protocol::result::{SolveOutcome, SolveResult};
use crate::region::catalog::Catalog;
use crate::region::resolver::{Pin, Resolver};
use crate::store::checkpoint::{CheckpointStore, PartitionFrontier, VehicleCheckpoint};

/// Static configuration for a [`PartitionWorker`].
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// The partition this worker owns.
    pub partition: u16,
    /// The scheduler's per-vehicle limits.
    pub scheduler: SchedulerConfig,
    /// The frontier persistence cadence.
    pub frontier: FrontierConfig,
    /// The dispatcher's continuity/publish knobs (mirror of the built one).
    pub dispatch: DispatchConfig,
    /// The committer's publish-retry policy (mirror of the built one).
    pub commit: CommitConfig,
    /// The deadline grace knob.
    pub deadline: DeadlineConfig,
    /// How often the housekeeping tick fires (frontier persist, retries,
    /// eviction).
    pub tick: Duration,
    /// How often idle vehicles are evicted.
    pub evict_every: Duration,
    /// How long a blocked vehicle waits before its prepared commit is retried.
    pub blocked_retry: Duration,
    /// How long shutdown waits for in-flight commits to quiesce.
    pub grace: Duration,
}

impl WorkerConfig {
    /// The spec defaults for `partition`: a 250 ms tick, 30 s eviction sweep,
    /// 5 s blocked-commit retry, and a 20 s shutdown grace.
    #[must_use]
    pub fn new(partition: u16) -> Self {
        Self {
            partition,
            scheduler: SchedulerConfig::default(),
            frontier: FrontierConfig::default(),
            dispatch: DispatchConfig::default(),
            commit: CommitConfig::default(),
            deadline: DeadlineConfig::default(),
            tick: Duration::from_millis(250),
            evict_every: Duration::from_secs(30),
            blocked_retry: Duration::from_secs(5),
            grace: Duration::from_secs(20),
        }
    }
}

/// A running tally of what a worker has processed, for the caller's exit log and
/// the metrics layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerStats {
    /// Raw deliveries seen.
    pub observed: u64,
    /// Observations appended to a vehicle FIFO.
    pub queued: u64,
    /// Duplicate transport deliveries coalesced.
    pub coalesced: u64,
    /// Valid-but-suppressed raw messages (includes `coalesced`).
    pub suppressed: u64,
    /// Poison raw messages acked and dropped.
    pub poison: u64,
    /// Jobs dispatched to the solve plane.
    pub dispatched: u64,
    /// Dispatches held by admission (or a transient publish fault).
    pub held: u64,
    /// Results accepted as the answer to an active job.
    pub accepted: u64,
    /// Early results parked for later replay.
    pub parked: u64,
    /// Obsolete results acked and dropped.
    pub rejected: u64,
    /// Same-identity results quarantined out of the commit path.
    pub quarantined: u64,
    /// Vehicles retired to the quarantine set after a permanent commit fault.
    pub quarantined_vehicles: u64,
    /// Commits made durable (matched or terminal alike).
    pub committed: u64,
    /// Terminal commits (no match).
    pub terminal: u64,
    /// Commits that opened a fresh segment (a reset).
    pub resets: u64,
    /// Commits abandoned to a concurrent decision (a store conflict).
    pub conflicts: u64,
    /// The current completion frontier.
    pub frontier: u64,
}

/// The per-vehicle provenance the worker keeps for the job currently in flight.
#[derive(Clone)]
struct ActiveMeta {
    /// The reset the commit must emit first, if continuity broke.
    reset: Option<ResetReason>,
    /// The continuity segment this job belongs to.
    segment: SegmentId,
    /// The serving region (for the next checkpoint and admission).
    region: RegionId,
    /// The graph snapshot (for the next checkpoint).
    graph: GraphVersion,
    /// The revision the commit compare-and-swaps against.
    expected_base: Option<Revision>,
}

/// A reset a restore reported, held until the first commit after the worker
/// re-acquires the vehicle applies it.
#[derive(Clone, Copy)]
struct PendingReset {
    /// Why continuity was broken.
    reason: ResetReason,
    /// The revision the reset's commit must compare-and-swap against, or `None`.
    prior: Option<Revision>,
}

/// The worker's own owned projection of [`Verdict`].
enum ResultVerdict {
    /// Commit this result as the answer to the active job.
    Accept,
    /// Early: park it and revisit when the job is dispatched.
    Park,
    /// Obsolete: ack and drop.
    Reject(RejectReason),
    /// A same-identity duplicate to keep out of the commit path.
    Quarantine(QuarantineReason),
}

impl From<Verdict<'_>> for ResultVerdict {
    fn from(verdict: Verdict<'_>) -> Self {
        match verdict {
            Verdict::Accept { .. } => ResultVerdict::Accept,
            Verdict::Park => ResultVerdict::Park,
            Verdict::Reject(reason) => ResultVerdict::Reject(reason),
            Verdict::Quarantine(reason) => ResultVerdict::Quarantine(reason),
        }
    }
}

/// What one [`PartitionWorker::try_dispatch`] attempt did.
enum DispatchOutcome {
    /// A job was published (or the observation was terminal-committed directly).
    Done,
    /// Admission (or a transient publish fault) held the dispatch; the caller
    /// should stop draining the ready queue this round.
    Held,
    /// Nothing to do (blocked, ineligible, or a transient restore failure).
    Skipped,
}

/// The single-task partition worker.
pub struct PartitionWorker<E, S, JP, OP, RS, XS>
where
    E: Entry,
    S: CheckpointStore,
    RS: Source<RawBytes>,
{
    cfg: WorkerConfig,
    catalog: Arc<Catalog>,
    admission: Admission,
    store: S,
    dispatcher: Dispatcher<JP>,
    committer: Committer<E, S, OP>,
    reader: RawReader,
    raw: RS,
    results: XS,
    scheduler: Scheduler<E, RS::Handle>,
    tracker: FrontierTracker,
    deadlines: Deadlines,
    /// Vehicles whose commit is stuck, mapped to the next retry instant.
    blocked: HashMap<VehicleId, Instant>,
    /// Admission-held vehicles retaining a [`Waiting`] guard until dispatch.
    held: HashMap<VehicleId, Waiting>,
    /// Per-vehicle provenance for the job currently in flight.
    active_meta: HashMap<VehicleId, ActiveMeta>,
    /// A pending `Reset { StateLost }`, applied on the next commit.
    pending_reset: HashMap<VehicleId, PendingReset>,
    /// Vehicles retired after a permanent commit fault.
    quarantined: HashSet<VehicleId>,
    shutdown: Shutdown,
    drain: Drain,
    last_evict: Instant,
    stats: WorkerStats,
    /// Bounded-label metrics; [`Metrics::noop`] by default until [`with_metrics`](PartitionWorker::with_metrics) installs a real handle.
    metrics: Metrics,
}

impl<E, S, JP, OP, RS, XS> PartitionWorker<E, S, JP, OP, RS, XS>
where
    E: Entry + serde::de::DeserializeOwned,
    S: CheckpointStore,
    JP: Publisher<SolveJob<E>>,
    OP: Publisher<CommittedOutput<E>>,
    RS: Source<RawBytes>,
    XS: Source<SolveResult<E>>,
{
    /// Build a worker for a partition the caller has *already recovered*.
    ///
    /// The caller must create `raw` starting at `report.frontier + 1`; the worker
    /// seeds its frontier from `report.frontier` and blocks `report.prepared_failed`.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        cfg: WorkerConfig,
        catalog: Arc<Catalog>,
        admission: Admission,
        store: S,
        dispatcher: Dispatcher<JP>,
        committer: Committer<E, S, OP>,
        raw: RS,
        results: XS,
        report: &RecoveryReport,
        shutdown: Shutdown,
    ) -> Self {
        let now = Instant::now();
        let initial = report.frontier.map(|sequence| PartitionFrontier {
            partition: cfg.partition,
            sequence,
        });
        let tracker = FrontierTracker::with_config(cfg.partition, initial, now, cfg.frontier);
        let scheduler = Scheduler::new(cfg.scheduler);
        let reader = RawReader::new(cfg.partition);

        let mut blocked = HashMap::new();
        for &vehicle in &report.prepared_failed {
            blocked.insert(vehicle, now);
        }

        Self {
            cfg,
            catalog,
            admission,
            store,
            dispatcher,
            committer,
            reader,
            raw,
            results,
            scheduler,
            tracker,
            deadlines: Deadlines::new(),
            blocked,
            held: HashMap::new(),
            active_meta: HashMap::new(),
            pending_reset: HashMap::new(),
            quarantined: HashSet::new(),
            shutdown,
            drain: Drain::new(),
            last_evict: now,
            stats: WorkerStats::default(),
            metrics: Metrics::noop(),
        }
    }

    /// Install a metrics handle so the worker records bounded-label instruments.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// Run the partition until shutdown, returning the run's [`WorkerStats`].
    pub async fn run(mut self) -> anyhow::Result<WorkerStats> {
        let mut tick = interval(self.cfg.tick);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut raw_open = true;
        let mut results_open = true;

        loop {
            let next_deadline = self.deadlines.next_at();
            tokio::select! {
                biased;
                () = self.shutdown.triggered() => break,
                maybe = self.raw.next(), if raw_open => match maybe {
                    Some(Ok(delivery)) => self.on_raw(delivery).await,
                    Some(Err(error)) => warn!(%error, "raw source error"),
                    None => raw_open = false,
                },
                maybe = self.results.next(), if results_open => match maybe {
                    Some(Ok(delivery)) => self.on_result(delivery).await,
                    Some(Err(error)) => warn!(%error, "result source error"),
                    None => results_open = false,
                },
                () = wait_deadline(next_deadline) => self.on_deadlines().await,
                _ = tick.tick() => self.on_tick().await,
            }
        }

        // Commits run inline, so the quiesce returns immediately.
        let _ = self.drain.quiesce(self.cfg.grace).await;
        let _ = self
            .store
            .set_frontier(PartitionFrontier {
                partition: self.cfg.partition,
                sequence: self.tracker.frontier(),
            })
            .await;
        self.stats.frontier = self.tracker.frontier();
        Ok(self.stats)
    }

    /// Classify one raw delivery through [`RawReader`], acting on its
    /// disposition, then drive any newly-ready vehicle.
    async fn on_raw(&mut self, delivery: Delivery<RawBytes, RS::Handle>) {
        self.stats.observed += 1;
        self.metrics
            .observed(&Metrics::partition_class(self.cfg.partition));
        let now = Instant::now();
        let reader = self.reader;
        let envelope = RawEnvelope {
            subject: &delivery.subject,
            headers: Some(&delivery.headers),
            bytes: delivery.item.0.as_slice(),
            handle: delivery.handle,
        };
        let disposition = reader.admit_bytes(&mut self.scheduler, &mut self.tracker, envelope, now);
        match disposition {
            RawDisposition::Queued { .. } => {
                self.stats.queued += 1;
                self.metrics.queued();
            }
            RawDisposition::Poison { handle, reason } => {
                self.stats.poison += 1;
                self.metrics.poison(reason.label());
                debug!(reason = reason.label(), "poison raw message");
                let seq = handle.sequence();
                let _ = handle.ack().await;
                self.tracker.complete(seq);
            }
            RawDisposition::Suppressed { handle, reason } => {
                self.stats.suppressed += 1;
                self.metrics.suppressed(reason.label());
                if reason == SuppressReason::Coalesced {
                    self.stats.coalesced += 1;
                }
                let seq = handle.sequence();
                let _ = handle.ack().await;
                // A coalesced duplicate is not terminal; the original owns its completion.
                if reason != SuppressReason::Coalesced {
                    self.tracker.complete(seq);
                }
            }
        }
        self.pump().await;
    }

    /// Handle one result delivery, then drive any newly-ready vehicle.
    async fn on_result(&mut self, delivery: Delivery<SolveResult<E>, XS::Handle>) {
        self.on_result_inner(delivery.item, Some(delivery.handle))
            .await;
        self.pump().await;
    }

    /// The shared result path, reused for both a freshly delivered result
    /// (`handle` is `Some`) and a parked one replayed after dispatch (`None`).
    async fn on_result_inner(&mut self, result: SolveResult<E>, handle: Option<XS::Handle>) {
        let vehicle = result.identity.vehicle_id;
        let now = Instant::now();
        match self.result_verdict(vehicle, &result, now) {
            ResultVerdict::Accept => {
                self.stats.accepted += 1;
                let Some(meta) = self.active_meta.get(&vehicle).cloned() else {
                    if let Some(handle) = handle {
                        let _ = handle.ack().await;
                    }
                    return;
                };
                self.metrics
                    .result(meta.region.as_str(), result.outcome.kind());
                if let Some(active) = self.scheduler.active(vehicle) {
                    self.metrics.round_trip_seconds(
                        meta.region.as_str(),
                        result.outcome.kind(),
                        now.saturating_duration_since(active.dispatched)
                            .as_secs_f64(),
                    );
                }
                let decision = if matches!(result.outcome, SolveOutcome::Solved { .. }) {
                    Decision::Solved {
                        result,
                        reset: meta.reset,
                        segment: meta.segment,
                    }
                } else {
                    let reason = result
                        .outcome
                        .terminal_reason()
                        .unwrap_or(TerminalReason::Internal);
                    Decision::Terminal {
                        job: result.job,
                        identity: result.identity.clone(),
                        reason,
                        closes_segment: false,
                        segment: meta.segment,
                    }
                };
                self.commit_decision(vehicle, decision, handle).await;
            }
            ResultVerdict::Park => {
                self.stats.parked += 1;
                self.metrics.parked();
                self.scheduler.park(vehicle, result, now);
                if let Some(handle) = handle {
                    let _ = handle.ack().await;
                }
            }
            ResultVerdict::Reject(reason) => {
                self.stats.rejected += 1;
                self.metrics.rejected(reason.label());
                debug!(
                    vehicle = vehicle.0,
                    reason = reason.label(),
                    "rejected result"
                );
                if let Some(handle) = handle {
                    let _ = handle.ack().await;
                }
            }
            ResultVerdict::Quarantine(reason) => {
                self.stats.quarantined += 1;
                self.metrics.quarantined(reason.label());
                warn!(
                    vehicle = vehicle.0,
                    reason = reason.label(),
                    "quarantined solve result: kept out of the commit path"
                );
                if let Some(handle) = handle {
                    let _ = handle.ack().await;
                }
            }
        }
    }

    /// Fire due deadlines: a live, uncommitted job whose timer expired commits a
    /// `Terminal` through the same commit path a real result uses.
    async fn on_deadlines(&mut self) {
        let now = Instant::now();
        for (vehicle, job) in self.deadlines.pop_expired(now) {
            let expiry = self
                .scheduler
                .state(vehicle)
                .map_or(Expiry::Ignore, |state| deadline::on_expiry(state, job));
            let Expiry::Terminal(reason) = expiry else {
                continue;
            };
            let Some(segment) = self.active_meta.get(&vehicle).map(|m| m.segment) else {
                continue;
            };
            let decision = {
                let Some(active) = self.scheduler.active(vehicle) else {
                    continue;
                };
                deadline::terminal_decision(active, reason, segment)
            };
            self.commit_decision(vehicle, decision, None).await;
        }
        self.pump().await;
    }

    /// Housekeeping: persist the frontier when due, retry blocked and held
    /// vehicles, and periodically evict idle ones.
    async fn on_tick(&mut self) {
        let now = Instant::now();

        self.metrics.frontier_lag(
            &Metrics::partition_class(self.cfg.partition),
            self.tracker.outstanding() as u64,
        );

        self.metrics.oldest_pending_seconds(
            self.scheduler
                .oldest_pending(now)
                .map_or(0.0, |age| age.as_secs_f64()),
        );

        let depth = self.scheduler.depth();
        self.metrics.depth(
            &Metrics::partition_class(self.cfg.partition),
            depth.vehicles as u64,
            depth.pending as u64,
            depth.active as u64,
        );

        if let Some(frontier) = self.tracker.due(now)
            && self.store.set_frontier(frontier).await.is_ok()
        {
            self.tracker.persisted(frontier.sequence, now);
            self.stats.frontier = self.tracker.frontier();
        }

        self.retry_blocked(now).await;

        if now.saturating_duration_since(self.last_evict) >= self.cfg.evict_every {
            self.last_evict = now;
            for vehicle in self.scheduler.evict_idle(now) {
                let _ = self.store.expire_idle(vehicle).await;
                self.active_meta.remove(&vehicle);
                self.held.remove(&vehicle);
                self.pending_reset.remove(&vehicle);
            }
        }

        self.retry_held().await;
        self.pump().await;
    }

    /// Drain the ready queue, dispatching each vehicle in turn and stopping at
    /// the first admission hold (nothing more can be dispatched this round).
    async fn pump(&mut self) {
        while let Some(vehicle) = self.scheduler.next_ready() {
            if let DispatchOutcome::Held = self.try_dispatch(vehicle).await {
                break;
            }
        }
    }

    /// Acquire, resolve, and dispatch one vehicle's head observation.
    async fn try_dispatch(&mut self, vehicle: VehicleId) -> DispatchOutcome {
        if self.quarantined.contains(&vehicle) {
            return DispatchOutcome::Skipped;
        }
        if self.blocked.contains_key(&vehicle) {
            return DispatchOutcome::Skipped;
        }
        // Eligible = has a head and no active job.
        if self.scheduler.active(vehicle).is_some() || self.scheduler.head(vehicle).is_none() {
            return DispatchOutcome::Skipped;
        }

        let now = Instant::now();

        if !self.scheduler.checkpoint(vehicle).is_loaded() {
            match restore_vehicle(&self.store, vehicle, self.cfg.partition, &self.committer).await {
                Ok(restored) => {
                    if let Some(reason) = restored.reset {
                        // Carry the stored revision so the reset's commit CASes against the stale checkpoint.
                        self.pending_reset.insert(
                            vehicle,
                            PendingReset {
                                reason,
                                prior: restored.prior,
                            },
                        );
                    }
                    self.scheduler.set_checkpoint(vehicle, restored.checkpoint);
                }
                Err(error) => {
                    warn!(vehicle = vehicle.0, %error, "restore failed; blocking vehicle");
                    self.blocked.insert(vehicle, now + self.cfg.blocked_retry);
                    return DispatchOutcome::Skipped;
                }
            }
        }

        let checkpoint = self.scheduler.checkpoint(vehicle).present().cloned();
        let head_point = self
            .scheduler
            .head(vehicle)
            .expect("an eligible vehicle has a head")
            .payload
            .point;
        let pin = checkpoint.as_ref().map(|cp| Pin {
            region: cp.region.clone(),
            graph: cp.graph.clone(),
            routing_version: self.catalog.routing_version,
        });

        let resolution = {
            let resolver = Resolver::new(&self.catalog);
            match resolver.resolve(vehicle, head_point, pin.as_ref()) {
                Ok(resolution) => resolution,
                Err(unserved) => {
                    self.commit_unserved(vehicle, checkpoint.as_ref(), unserved.cell)
                        .await;
                    return DispatchOutcome::Done;
                }
            }
        };

        let now_us = unix_micros();
        let dispatched = {
            let head = self.scheduler.head(vehicle).expect("head");
            self.dispatcher
                .dispatch::<E, RS::Handle>(
                    vehicle,
                    head,
                    checkpoint.as_ref(),
                    &resolution,
                    &self.admission,
                    now,
                    now_us,
                )
                .await
        };

        match dispatched {
            Ok(d) => {
                let pending = self.pending_reset.remove(&vehicle);
                let reset = d.reset.or(pending.map(|p| p.reason));
                // Fall back to the restore's stored `prior` so a state-lost reset CASes against the stale revision, not `None`.
                let expected_base = d
                    .job
                    .identity
                    .base
                    .map(|b| b.revision)
                    .or_else(|| pending.and_then(|p| p.prior));
                let fire = self.cfg.deadline.fire_at(d.job.deadline);
                let job_id = d.job.id;
                let job_bytes = d.job.bytes;
                self.active_meta.insert(
                    vehicle,
                    ActiveMeta {
                        reset,
                        segment: d.segment,
                        region: resolution.region.clone(),
                        graph: resolution.graph.clone(),
                        expected_base,
                    },
                );
                if self.scheduler.activate(vehicle, d.job).is_err() {
                    self.active_meta.remove(&vehicle);
                    return DispatchOutcome::Skipped;
                }
                self.deadlines.arm(fire, vehicle, job_id);
                self.held.remove(&vehicle);
                self.stats.dispatched += 1;
                self.metrics
                    .dispatched(resolution.region.as_str(), resolution.lane.0);
                self.metrics
                    .job_bytes(resolution.region.as_str(), job_bytes);

                for result in self.scheduler.take_parked(vehicle) {
                    self.on_result_inner(result, None).await;
                }
                DispatchOutcome::Done
            }
            Err(DispatchError::Held(reason)) => {
                self.stats.held += 1;
                self.metrics.held(resolution.region.as_str());
                debug!(vehicle = vehicle.0, reason = %reason, "dispatch held by admission");
                self.held
                    .insert(vehicle, self.admission.hold(&resolution.region));
                DispatchOutcome::Held
            }
            Err(error) => {
                warn!(vehicle = vehicle.0, %error, "dispatch failed; will retry");
                self.held
                    .insert(vehicle, self.admission.hold(&resolution.region));
                DispatchOutcome::Held
            }
        }
    }

    /// Commit `decision` for `vehicle` and advance past its head. The single
    /// funnel both the result path and the deadline path reach.
    async fn commit_decision(
        &mut self,
        vehicle: VehicleId,
        decision: Decision<E>,
        result_handle: Option<XS::Handle>,
    ) {
        let _guard = self.drain.begin();
        let now = Instant::now();
        let now_us = unix_micros();

        let Some(meta) = self.active_meta.get(&vehicle).cloned() else {
            if let Some(handle) = result_handle {
                let _ = handle.ack().await;
            }
            return;
        };

        // Arbitration point: the first to prepare wins.
        if self.scheduler.begin_commit(vehicle).is_err() {
            if let Some(handle) = result_handle {
                let _ = handle.ack().await;
            }
            return;
        }

        let raw = self
            .scheduler
            .active(vehicle)
            .expect("begin_commit succeeded, so a job is active")
            .observation;
        let prev = self.scheduler.checkpoint(vehicle).present().cloned();

        let (reset_opt, segment) = decision_reset_segment(&decision);
        debug_assert!(
            reset_opt.is_some() || prev.as_ref().is_none_or(|p| p.segment == segment),
            "a commit must not change the segment without a reset",
        );
        let is_terminal = matches!(decision, Decision::Terminal { .. });
        let terminal_reason = match &decision {
            Decision::Terminal { reason, .. } => Some(reason.label()),
            _ => None,
        };
        let reset_reason = reset_opt.map(|reason| reason.label());

        let plan = commit::plan(
            prev.as_ref(),
            decision,
            raw,
            &meta.region,
            &meta.graph,
            now_us,
        );
        let next = plan.next.clone();

        let commit_start = Instant::now();
        match self
            .committer
            .commit(vehicle, self.cfg.partition, plan, meta.expected_base, raw)
            .await
        {
            Ok(committed) => {
                self.scheduler
                    .set_checkpoint(vehicle, CheckpointState::Present(next));
                if let Ok(finished) = self.scheduler.finish(vehicle, now) {
                    let seq = finished.observation.id.sequence;
                    let _ = finished.observation.handle.ack().await;
                    if let Some(handle) = result_handle {
                        let _ = handle.ack().await;
                    }
                    self.tracker.complete(seq);
                    drop(finished.job); // releases the admission permit
                }
                self.active_meta.remove(&vehicle);
                self.pending_reset.remove(&vehicle);
                self.blocked.remove(&vehicle);
                self.stats.committed += 1;
                let kind = if is_terminal { "terminal" } else { "matched" };
                self.metrics
                    .commit_seconds(kind, commit_start.elapsed().as_secs_f64());
                self.metrics.output_bytes(committed.bytes as u64);
                // A reset commit emits the reset before the matched/terminal output.
                if let Some(reason) = reset_reason {
                    self.metrics.completion("reset", reason);
                }
                if is_terminal {
                    self.metrics
                        .completion("terminal", terminal_reason.unwrap_or("internal"));
                } else {
                    self.metrics.completion("matched", "solved");
                }
                if is_terminal {
                    self.stats.terminal += 1;
                }
                if reset_opt.is_some() {
                    self.stats.resets += 1;
                }
                self.stats.frontier = self.tracker.frontier();
            }
            Err(CommitError::Conflict { actual }) => {
                // A concurrent decision won the race; reload and block for the tick retry.
                self.stats.conflicts += 1;
                warn!(
                    vehicle = vehicle.0,
                    ?actual,
                    "commit conflicted; blocking vehicle"
                );
                if let Ok(restored) =
                    restore_vehicle(&self.store, vehicle, self.cfg.partition, &self.committer).await
                {
                    self.scheduler.set_checkpoint(vehicle, restored.checkpoint);
                }
                self.blocked.insert(vehicle, now + self.cfg.blocked_retry);
            }
            Err(error) if is_permanent_commit_error(&error) => {
                self.quarantine_vehicle(vehicle, &error);
            }
            Err(error) => {
                warn!(vehicle = vehicle.0, %error, "commit incomplete; blocking vehicle");
                self.blocked.insert(vehicle, now + self.cfg.blocked_retry);
            }
        }
    }

    /// Retire a vehicle whose commit failed with a permanent encode/decode fault.
    ///
    /// The head observation is left un-acked, so the frontier never advances past it.
    fn quarantine_vehicle(&mut self, vehicle: VehicleId, error: &CommitError<S::Error>) {
        error!(
            vehicle = vehicle.0,
            %error,
            "commit failed permanently; quarantining vehicle for this process lifetime"
        );
        self.blocked.remove(&vehicle);
        self.pending_reset.remove(&vehicle);
        self.held.remove(&vehicle);
        if self.quarantined.insert(vehicle) {
            self.stats.quarantined_vehicles += 1;
        }
    }

    /// Commit an `UnsupportedCoverage` terminal for a vehicle no region serves.
    ///
    /// A synthetic zero-byte permit and job stand in so the scheduler can advance
    /// past the head; the permit is released on every exit path.
    async fn commit_unserved(
        &mut self,
        vehicle: VehicleId,
        checkpoint: Option<&VehicleCheckpoint<E>>,
        cell: String,
    ) {
        let now = Instant::now();
        let head_obs = self
            .scheduler
            .head(vehicle)
            .expect("an eligible vehicle has a head")
            .id;

        let (region, graph) = match checkpoint {
            Some(cp) => (cp.region.clone(), cp.graph.clone()),
            None => match self.catalog.regions.first() {
                Some(region) => (region.id.clone(), region.graph.clone()),
                None => {
                    warn!(vehicle = vehicle.0, %cell, "unserved cell and empty catalog; cannot advance");
                    return;
                }
            },
        };
        let Ok(permit) = self.admission.try_admit(&region, 0) else {
            warn!(vehicle = vehicle.0, %cell, "unserved cell and admission full; cannot advance");
            return;
        };

        let segment = checkpoint.map_or(SegmentId::from(head_obs), |cp| cp.segment);
        let base = checkpoint.map(|cp| BaseState {
            revision: cp.revision,
            segment: cp.segment,
        });
        let identity = JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: vehicle,
            observation: head_obs,
            base,
            graph: graph.clone(),
            region: region.clone(),
        };
        let job_id = identity.job_id();
        // Fall back to the stored `prior` so a state-lost terminal CASes against the stale revision, not `None`.
        let expected_base = base
            .map(|b| b.revision)
            .or_else(|| self.pending_reset.get(&vehicle).and_then(|p| p.prior));
        self.active_meta.insert(
            vehicle,
            ActiveMeta {
                reset: None,
                segment,
                region,
                graph,
                expected_base,
            },
        );
        let job = ActiveJob {
            id: job_id,
            identity: identity.clone(),
            observation: head_obs,
            deadline: now,
            bytes: 0,
            permit,
            dispatched: now,
        };
        if self.scheduler.activate(vehicle, job).is_err() {
            self.active_meta.remove(&vehicle);
            return;
        }
        self.pending_reset.remove(&vehicle);

        debug!(vehicle = vehicle.0, %cell, "committing unsupported-coverage terminal");
        let decision = Decision::Terminal {
            job: job_id,
            identity,
            reason: TerminalReason::UnsupportedCoverage,
            closes_segment: false,
            segment,
        };
        self.commit_decision(vehicle, decision, None).await;
    }

    /// Retry every blocked vehicle whose backoff has elapsed, re-driving its
    /// surviving prepared commit through [`Committer::finish_prepared`].
    async fn retry_blocked(&mut self, now: Instant) {
        let due: Vec<VehicleId> = self
            .blocked
            .iter()
            .filter(|&(_, &at)| at <= now)
            .map(|(&vehicle, _)| vehicle)
            .collect();

        for vehicle in due {
            let prepared = match self.store.load(vehicle).await {
                Ok((_, prepared)) => prepared,
                Err(error) => {
                    warn!(vehicle = vehicle.0, %error, "blocked-vehicle load failed");
                    self.blocked.insert(vehicle, now + self.cfg.blocked_retry);
                    continue;
                }
            };

            match prepared {
                Some(prepared) => {
                    match self
                        .committer
                        .finish_prepared(vehicle, self.cfg.partition, prepared)
                        .await
                    {
                        Ok(_) => self.resolve_blocked(vehicle, now).await,
                        Err(error) if is_permanent_commit_error(&error) => {
                            self.quarantine_vehicle(vehicle, &error);
                        }
                        Err(error) => {
                            debug!(vehicle = vehicle.0, %error, "blocked commit still cannot finish");
                            self.blocked.insert(vehicle, now + self.cfg.blocked_retry);
                        }
                    }
                }
                None => self.resolve_blocked(vehicle, now).await,
            }
        }
    }

    /// Finish unblocking a vehicle whose surviving commit has been resolved:
    /// advance past its head (if a committing job is still attached) and clear
    /// its bookkeeping so its next observation restores fresh state.
    async fn resolve_blocked(&mut self, vehicle: VehicleId, now: Instant) {
        if self.scheduler.active(vehicle).is_some() {
            // Force a reload of the promoted checkpoint on the next dispatch.
            self.scheduler
                .set_checkpoint(vehicle, CheckpointState::Unloaded);
            if let Ok(finished) = self.scheduler.finish(vehicle, now) {
                let seq = finished.observation.id.sequence;
                let _ = finished.observation.handle.ack().await;
                self.tracker.complete(seq);
                drop(finished.job);
            }
        }
        self.active_meta.remove(&vehicle);
        self.blocked.remove(&vehicle);
        self.stats.frontier = self.tracker.frontier();
    }

    /// Re-attempt dispatch for every admission-held vehicle.
    async fn retry_held(&mut self) {
        let held: Vec<VehicleId> = self.held.keys().copied().collect();
        for vehicle in held {
            if self.blocked.contains_key(&vehicle) {
                continue;
            }
            if let DispatchOutcome::Held = self.try_dispatch(vehicle).await {
                // Still held: stop hammering admission this round.
                break;
            }
        }
    }

    /// Classify one result by reusing the pure
    /// [`validate::validate`](crate::orchestrator::validate::validate), projecting
    /// its borrow-carrying [`Verdict`] into the worker's owned [`ResultVerdict`].
    fn result_verdict(
        &self,
        vehicle: VehicleId,
        result: &SolveResult<E>,
        now: Instant,
    ) -> ResultVerdict {
        match self.scheduler.state(vehicle) {
            Some(state) => validate::validate(state, result, self.cfg.partition).into(),
            None => {
                let detached = detached_state::<E, XS::Handle>(now);
                validate::validate(&detached, result, self.cfg.partition).into()
            }
        }
    }
}

/// Whether a commit failed permanently — an encode or decode fault a retry would only reproduce.
fn is_permanent_commit_error<SE>(error: &CommitError<SE>) -> bool {
    matches!(error, CommitError::Decode(_) | CommitError::Encode(_))
}

/// A detached, empty [`VehicleState`] for validating a result for an untracked vehicle.
fn detached_state<E: Entry, H: AckHandle>(now: Instant) -> VehicleState<E, H> {
    VehicleState {
        checkpoint: CheckpointState::Unloaded,
        pending: VecDeque::new(),
        active: None,
        committing: false,
        parked: Vec::new(),
        last_touch: now,
    }
}

/// The `(reset, segment)` a decision carries, read without consuming it.
fn decision_reset_segment<E: Entry>(decision: &Decision<E>) -> (Option<ResetReason>, SegmentId) {
    match decision {
        Decision::Solved { reset, segment, .. } => (*reset, *segment),
        Decision::Terminal { segment, .. } => (None, *segment),
        Decision::Reset {
            reason,
            new_segment,
            ..
        } => (Some(*reason), *new_segment),
    }
}

/// Sleep until `at`, or forever when no deadline is armed.
async fn wait_deadline(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => core::future::pending::<()>().await,
    }
}

/// The current wall clock as absolute unix microseconds, saturating rather than wrapping.
fn unix_micros() -> i64 {
    crate::bus::wallclock()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_micros()).ok())
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_nats::HeaderMap;
    use chrono::{DateTime, Utc};
    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_transition::matcher::Trip;

    use crate::bus::Wire;
    use crate::bus::memory::{MemoryBus, MemoryPublisher, MemorySource};
    use crate::event::{MatchedDiff, Payload, shard_of};
    use crate::orchestrator::admission::AdmissionConfig;
    use crate::partition::partition_of;
    use crate::protocol::ids::{JobId, ObservationId, OutputId};
    use crate::protocol::output::OutputKind;
    use crate::store::checkpoint::{CommitPhase, MemoryCheckpointStore, PreparedCommit};
    use crate::topology::{output_subject, raw_subject, result_subject};

    type E = MockEntryId;
    type Worker = PartitionWorker<
        E,
        MemoryCheckpointStore,
        MemoryPublisher<SolveJob<E>>,
        MemoryPublisher<CommittedOutput<E>>,
        MemorySource<RawBytes>,
        MemorySource<SolveResult<E>>,
    >;

    /// A Sydney fix; its precision-4 cell is what the test catalog serves.
    fn point() -> Point {
        Point::new(151.2093, -33.8688)
    }

    /// A one-region catalog serving the fixture cell, with a tunable freshness budget.
    fn catalog(budget_ms: u64) -> Catalog {
        let cell = shard_of(point()).to_string();
        let toml = format!(
            r#"
version = 1
routing_version = 1

[[regions]]
id = "r1"
graph = "g1"
coverage = ["{cell}"]
overlap = []
lanes = 1
resource_class = "c"
replicas = {{ min = 1, max = 1 }}
freshness_budget_ms = {budget_ms}
"#,
        );
        Catalog::parse(&toml).expect("catalog parses")
    }

    /// A test config: a brisk tick and blocked-retry so timers resolve fast
    /// under `start_paused`.
    fn config(partition: u16) -> WorkerConfig {
        WorkerConfig {
            tick: Duration::from_millis(20),
            blocked_retry: Duration::from_millis(20),
            ..WorkerConfig::new(partition)
        }
    }

    fn admission_for(catalog: &Catalog) -> Admission {
        Admission::new(
            AdmissionConfig::default(),
            catalog.regions.iter().map(|r| &r.id),
        )
    }

    fn clean_report(partition: u16) -> RecoveryReport {
        RecoveryReport {
            partition,
            frontier: None,
            prepared_found: 0,
            prepared_finished: 0,
            prepared_failed: Vec::new(),
        }
    }

    fn build_worker(
        cfg: WorkerConfig,
        catalog: Arc<Catalog>,
        bus: &MemoryBus,
        store: &MemoryCheckpointStore,
        shutdown: Shutdown,
    ) -> Worker {
        let admission = admission_for(&catalog);
        let report = clean_report(cfg.partition);
        build_worker_with(cfg, catalog, bus, store, admission, &report, shutdown)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_worker_with(
        cfg: WorkerConfig,
        catalog: Arc<Catalog>,
        bus: &MemoryBus,
        store: &MemoryCheckpointStore,
        admission: Admission,
        report: &RecoveryReport,
        shutdown: Shutdown,
    ) -> Worker {
        let dispatcher = Dispatcher::new(bus.publisher::<SolveJob<E>>(), DispatchConfig::default());
        let committer = Committer::new(
            store.clone(),
            bus.publisher::<CommittedOutput<E>>(),
            CommitConfig {
                publish_attempts: 4,
                backoff: Duration::ZERO,
            },
        );
        let raw = bus.source::<RawBytes>(&raw_subject(u64::from(cfg.partition)));
        let results = bus.source::<SolveResult<E>>(&result_subject(u64::from(cfg.partition)));
        PartitionWorker::new(
            cfg,
            catalog,
            admission,
            store.clone(),
            dispatcher,
            committer,
            raw,
            results,
            report,
            shutdown,
        )
    }

    fn partition_for(vehicle: u64) -> u16 {
        partition_of(VehicleId(vehicle)) as u16
    }

    fn same_partition_as(vehicle: u64) -> u64 {
        let want = partition_for(vehicle);
        (vehicle + 1..)
            .find(|&v| partition_for(v) == want)
            .expect("another vehicle in the partition")
    }

    async fn publish_raw(bus: &MemoryBus, partition: u16, vehicle: u64, ts_us: i64) -> u64 {
        let payload = Payload {
            vehicle_id: VehicleId(vehicle),
            timestamp: DateTime::<Utc>::from_timestamp_micros(ts_us).unwrap(),
            point: point(),
        };
        let bytes = payload.encode().unwrap();
        let msg_id = format!("{vehicle}:{ts_us}");
        let outcome = bus
            .publisher::<RawBytes>()
            .publish_bytes(
                &raw_subject(u64::from(partition)),
                &msg_id,
                HeaderMap::new(),
                &bytes,
            )
            .await
            .expect("raw publish");
        match outcome {
            crate::bus::adapter::PublishOutcome::Acked { sequence, .. } => sequence,
        }
    }

    /// A scripted matcher: every job gets a `Solved` result with an empty diff.
    async fn run_matcher(bus: MemoryBus, shutdown: Shutdown) {
        let mut jobs = bus.source::<SolveJob<E>>("solve.v1.g.>");
        let publisher = bus.publisher::<SolveResult<E>>();
        loop {
            tokio::select! {
                biased;
                () = shutdown.triggered() => break,
                maybe = jobs.next() => match maybe {
                    Some(Ok(delivery)) => {
                        let job = delivery.item;
                        let outcome = SolveOutcome::Solved {
                            diff: MatchedDiff {
                                revision: 1,
                                downgraded: false,
                                layers: Vec::new(),
                            },
                            trip: Trip::new(),
                            converged_through: None,
                        };
                        let result = SolveResult::new(&job, outcome, 0);
                        let subject = result_subject(u64::from(result.partition()));
                        let _ = publisher
                            .publish(&subject, &result.msg_id(), HeaderMap::new(), &result)
                            .await;
                        let _ = delivery.handle.ack().await;
                    }
                    _ => break,
                },
            }
        }
    }

    async fn wait_until(cond: impl Fn() -> bool) {
        for _ in 0..2000 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("condition never held");
    }

    #[tokio::test(start_paused = true)]
    async fn happy_path_three_observations_one_vehicle() {
        let vehicle = 1u64;
        let partition = partition_for(vehicle);
        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        // Published first so their sequences are 1, 2, 3.
        let mut seqs = Vec::new();
        for i in 0..3 {
            seqs.push(
                publish_raw(
                    &bus,
                    partition,
                    vehicle,
                    1_775_000_000_000_000 + i * 1_000_000,
                )
                .await,
            );
        }
        let last_seq = *seqs.last().unwrap();

        let worker = build_worker(
            config(partition),
            Arc::new(catalog(30_000)),
            &bus,
            &store,
            shutdown.clone(),
        );

        let out_subject = output_subject(u64::from(partition));
        let driver = {
            let bus = bus.clone();
            let shutdown = shutdown.clone();
            let out_subject = out_subject.clone();
            async move {
                wait_until(|| bus.published(&out_subject).len() >= 3).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Operator);
            }
        };

        let (stats, (), ()) = tokio::join!(
            worker.run(),
            run_matcher(bus.clone(), shutdown.clone()),
            driver
        );
        let stats = stats.expect("worker ran");

        assert_eq!(stats.dispatched, 3, "three jobs dispatched in order");
        assert_eq!(stats.committed, 3, "three commits");
        assert_eq!(stats.accepted, 3);
        assert_eq!(
            stats.frontier, last_seq,
            "frontier at the last raw sequence"
        );
        assert_eq!(bus.published(&out_subject).len(), 3, "three outputs");

        let (checkpoint, prepared) = store.load(VehicleId(vehicle)).await.unwrap();
        assert_eq!(checkpoint.unwrap().revision, Revision(last_seq));
        assert!(
            prepared.is_none(),
            "no prepared record survives an idle vehicle"
        );
        assert_eq!(bus.acked_count(&raw_subject(u64::from(partition))), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn two_vehicles_interleave_without_blocking() {
        let a = 1u64;
        let b = same_partition_as(a);
        let partition = partition_for(a);
        assert_eq!(partition_for(b), partition);

        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        publish_raw(&bus, partition, a, 1_775_000_000_000_000).await;
        publish_raw(&bus, partition, b, 1_775_000_000_000_000).await;
        publish_raw(&bus, partition, a, 1_775_000_001_000_000).await;
        publish_raw(&bus, partition, b, 1_775_000_001_000_000).await;

        let worker = build_worker(
            config(partition),
            Arc::new(catalog(30_000)),
            &bus,
            &store,
            shutdown.clone(),
        );

        let out_subject = output_subject(u64::from(partition));
        let driver = {
            let bus = bus.clone();
            let shutdown = shutdown.clone();
            let out_subject = out_subject.clone();
            async move {
                wait_until(|| bus.published(&out_subject).len() >= 4).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Operator);
            }
        };

        let (stats, (), ()) = tokio::join!(
            worker.run(),
            run_matcher(bus.clone(), shutdown.clone()),
            driver
        );
        let stats = stats.expect("worker ran");

        assert_eq!(stats.committed, 4, "both vehicles fully commit");
        assert!(store.load(VehicleId(a)).await.unwrap().0.is_some());
        assert!(store.load(VehicleId(b)).await.unwrap().0.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_terminal_and_late_result_rejected() {
        let vehicle = 1u64;
        let partition = partition_for(vehicle);
        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        let seq = publish_raw(&bus, partition, vehicle, 1_775_000_000_000_000).await;

        // No matcher runs, so the job is never answered; a short budget expires it fast.
        let worker = build_worker(
            config(partition),
            Arc::new(catalog(100)),
            &bus,
            &store,
            shutdown.clone(),
        );

        let out_subject = output_subject(u64::from(partition));
        let driver = {
            let bus = bus.clone();
            let shutdown = shutdown.clone();
            let out_subject = out_subject.clone();
            async move {
                wait_until(|| !bus.published(&out_subject).is_empty()).await;
                let identity = JobIdentity {
                    schema: SCHEMA_VERSION,
                    vehicle_id: VehicleId(vehicle),
                    observation: crate::protocol::ids::ObservationId {
                        partition,
                        sequence: seq,
                    },
                    base: None,
                    graph: GraphVersion::new("g1").unwrap(),
                    region: RegionId::new("r1").unwrap(),
                };
                let late = SolveResult::<E> {
                    job: identity.job_id(),
                    identity,
                    outcome: SolveOutcome::Unanchored,
                    solved_at_us: 0,
                };
                bus.publisher::<SolveResult<E>>()
                    .publish(
                        &result_subject(u64::from(partition)),
                        &late.msg_id(),
                        HeaderMap::new(),
                        &late,
                    )
                    .await
                    .unwrap();
                wait_until(|| bus.acked_count(&result_subject(u64::from(partition))) >= 1).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Operator);
            }
        };

        let (stats, ()) = tokio::join!(worker.run(), driver);
        let stats = stats.expect("worker ran");

        assert_eq!(stats.terminal, 1, "the deadline committed one terminal");
        assert_eq!(stats.committed, 1);
        assert_eq!(stats.rejected, 1, "the late result was rejected as decided");
        assert_eq!(bus.published(&out_subject).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn duplicate_raw_is_coalesced_and_acked() {
        let vehicle = 1u64;
        let partition = partition_for(vehicle);
        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        publish_raw(&bus, partition, vehicle, 1_775_000_000_000_000).await;

        // No matcher: the observation stays in flight, so a redelivery coalesces against the original.
        let worker = build_worker(
            config(partition),
            Arc::new(catalog(30_000)),
            &bus,
            &store,
            shutdown.clone(),
        );

        let job_filter = "solve.v1.g.>";
        let raw_sub = raw_subject(u64::from(partition));
        let driver = {
            let bus = bus.clone();
            let shutdown = shutdown.clone();
            let raw_sub = raw_sub.clone();
            async move {
                wait_until(|| !bus.published(job_filter).is_empty()).await;
                bus.redeliver_unacked(&raw_sub);
                wait_until(|| bus.acked_count(&raw_sub) >= 1).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Operator);
            }
        };

        let (stats, ()) = tokio::join!(worker.run(), driver);
        let stats = stats.expect("worker ran");

        assert_eq!(stats.observed, 2, "the observation was delivered twice");
        assert_eq!(stats.coalesced, 1, "the redelivery coalesced");
        assert_eq!(stats.dispatched, 1, "only one job was dispatched");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_leaves_no_prepared_records_when_idle() {
        let vehicle = 1u64;
        let partition = partition_for(vehicle);
        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        publish_raw(&bus, partition, vehicle, 1_775_000_000_000_000).await;
        publish_raw(&bus, partition, vehicle, 1_775_000_001_000_000).await;

        let worker = build_worker(
            config(partition),
            Arc::new(catalog(30_000)),
            &bus,
            &store,
            shutdown.clone(),
        );

        let out_subject = output_subject(u64::from(partition));
        let driver = {
            let bus = bus.clone();
            let shutdown = shutdown.clone();
            let out_subject = out_subject.clone();
            async move {
                wait_until(|| bus.published(&out_subject).len() >= 2).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Signal);
            }
        };

        let (stats, (), ()) = tokio::join!(
            worker.run(),
            run_matcher(bus.clone(), shutdown.clone()),
            driver
        );
        let stats = stats.expect("worker ran");

        assert_eq!(stats.committed, 2);
        assert!(
            store.snapshot().prepared.is_empty(),
            "no prepared records remain"
        );
    }

    /// Install a committed checkpoint whose stored `bytes` are `garbage` at revision `rev`.
    async fn seed_checkpoint_bytes(
        store: &MemoryCheckpointStore,
        vehicle: u64,
        partition: u16,
        rev: u64,
        garbage: Vec<u8>,
    ) {
        let seed_bus = MemoryBus::new();
        let obs = ObservationId {
            partition,
            sequence: rev,
        };
        let out_subject = output_subject(u64::from(partition));
        let output = CommittedOutput::<E>::new(
            JobId(u128::from(rev)),
            VehicleId(vehicle),
            obs,
            Revision(rev),
            SegmentId(rev),
            OutputKind::Terminal {
                reason: TerminalReason::Unanchored,
                closes_segment: false,
            },
        );
        let entries: Vec<(String, String, Vec<u8>)> = vec![(
            out_subject.clone(),
            output.msg_id(),
            output.encode().unwrap(),
        )];
        let prepared = PreparedCommit {
            output: output.id,
            output_subject: out_subject,
            output_bytes: postcard::to_allocvec(&entries).unwrap(),
            next_checkpoint: garbage,
            next_revision: Revision(rev),
            next_segment: SegmentId(rev),
            expected_base: None,
            phase: CommitPhase::Prepared,
            raw: obs,
        };
        store
            .prepare(VehicleId(vehicle), partition, prepared.clone())
            .await
            .unwrap();
        let committer = Committer::new(
            store.clone(),
            seed_bus.publisher::<CommittedOutput<E>>(),
            CommitConfig {
                publish_attempts: 4,
                backoff: Duration::ZERO,
            },
        );
        committer
            .finish_prepared(VehicleId(vehicle), partition, prepared)
            .await
            .expect("seed installs the checkpoint bytes");
    }

    #[tokio::test(start_paused = true)]
    async fn state_lost_reset_supersedes_the_stale_checkpoint_without_looping() {
        let vehicle = 1u64;
        let partition = partition_for(vehicle);
        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        // The store holds an undecodable checkpoint at revision 5 (state lost).
        seed_checkpoint_bytes(&store, vehicle, partition, 5, b"not a checkpoint".to_vec()).await;

        publish_raw(&bus, partition, vehicle, 1_775_000_000_000_000).await;

        let worker = build_worker(
            config(partition),
            Arc::new(catalog(30_000)),
            &bus,
            &store,
            shutdown.clone(),
        );

        let out_subject = output_subject(u64::from(partition));
        let driver = {
            let bus = bus.clone();
            let shutdown = shutdown.clone();
            let out_subject = out_subject.clone();
            async move {
                wait_until(|| bus.published(&out_subject).len() >= 2).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Operator);
            }
        };

        let (stats, (), ()) = tokio::join!(
            worker.run(),
            run_matcher(bus.clone(), shutdown.clone()),
            driver
        );
        let stats = stats.expect("worker ran");

        assert_eq!(stats.committed, 1, "exactly one commit, no loop");
        assert_eq!(stats.resets, 1, "the commit opened a fresh segment");
        assert_eq!(stats.conflicts, 0, "the CAS targeted the stale revision");
        assert_eq!(stats.quarantined_vehicles, 0);

        let published = bus.published(&out_subject);
        assert_eq!(published.len(), 2, "a reset then a match");
        let first = CommittedOutput::<E>::decode(&published[0].2).unwrap();
        match first.kind {
            OutputKind::Reset { reason, .. } => assert_eq!(reason, ResetReason::StateLost),
            other => panic!("expected a StateLost reset first, got {}", other.kind()),
        }
        let second = CommittedOutput::<E>::decode(&published[1].2).unwrap();
        assert!(
            matches!(second.kind, OutputKind::Matched { .. }),
            "the match follows the reset"
        );

        let (checkpoint, prepared) = store.load(VehicleId(vehicle)).await.unwrap();
        assert!(checkpoint.is_some(), "a fresh checkpoint was promoted");
        assert!(prepared.is_none(), "nothing is left staged");
        assert_eq!(bus.acked_count(&raw_subject(u64::from(partition))), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_poison_prepared_record_quarantines_the_vehicle_instead_of_spinning() {
        let vehicle = 1u64;
        let partition = partition_for(vehicle);
        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        // The staged output bytes will never decode.
        let prepared = PreparedCommit {
            output: OutputId(1),
            output_subject: output_subject(u64::from(partition)),
            output_bytes: vec![0xff, 0xff, 0xff, 0xff],
            next_checkpoint: Vec::new(),
            next_revision: Revision(1),
            next_segment: SegmentId(1),
            expected_base: None,
            phase: CommitPhase::Prepared,
            raw: ObservationId {
                partition,
                sequence: 1,
            },
        };
        store
            .prepare(VehicleId(vehicle), partition, prepared)
            .await
            .unwrap();

        // Start blocked, so the housekeeping tick drives `finish_prepared` and hits the decode.
        let admission = admission_for(&catalog(30_000));
        let report = RecoveryReport {
            partition,
            frontier: None,
            prepared_found: 1,
            prepared_finished: 0,
            prepared_failed: vec![VehicleId(vehicle)],
        };
        let worker = build_worker_with(
            config(partition),
            Arc::new(catalog(30_000)),
            &bus,
            &store,
            admission,
            &report,
            shutdown.clone(),
        );

        let driver = {
            let shutdown = shutdown.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Operator);
            }
        };

        let (stats, ()) = tokio::join!(worker.run(), driver);
        let stats = stats.expect("worker ran");

        assert_eq!(
            stats.quarantined_vehicles, 1,
            "the poison vehicle was quarantined exactly once"
        );
        assert_eq!(stats.committed, 0, "nothing could be committed");
        assert!(
            store.snapshot().prepared.contains_key(&VehicleId(vehicle)),
            "the undecodable record is left in place"
        );
    }

    /// A one-region catalog that serves a cell far from `point()`, so `point()` resolves to no region.
    fn unserved_catalog() -> Catalog {
        let elsewhere = shard_of(Point::new(0.0, 0.0)).to_string();
        assert_ne!(elsewhere, shard_of(point()).to_string());
        let toml = format!(
            r#"
version = 1
routing_version = 1

[[regions]]
id = "r1"
graph = "g1"
coverage = ["{elsewhere}"]
overlap = []
lanes = 1
resource_class = "c"
replicas = {{ min = 1, max = 1 }}
freshness_budget_ms = 30000
"#,
        );
        Catalog::parse(&toml).expect("catalog parses")
    }

    #[tokio::test(start_paused = true)]
    async fn an_unserved_point_yields_one_terminal_and_leaves_admission_at_zero() {
        let vehicle = 1u64;
        let partition = partition_for(vehicle);
        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        publish_raw(&bus, partition, vehicle, 1_775_000_000_000_000).await;

        let catalog = Arc::new(unserved_catalog());
        let admission = admission_for(&catalog);
        let admission_probe = admission.clone();
        let report = clean_report(partition);
        let worker = build_worker_with(
            config(partition),
            catalog,
            &bus,
            &store,
            admission,
            &report,
            shutdown.clone(),
        );

        let out_subject = output_subject(u64::from(partition));
        let driver = {
            let bus = bus.clone();
            let shutdown = shutdown.clone();
            let out_subject = out_subject.clone();
            async move {
                wait_until(|| !bus.published(&out_subject).is_empty()).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Operator);
            }
        };

        let (stats, ()) = tokio::join!(worker.run(), driver);
        let stats = stats.expect("worker ran");

        assert_eq!(stats.terminal, 1, "one terminal was committed");
        assert_eq!(stats.committed, 1);
        assert_eq!(stats.dispatched, 0, "no real job was dispatched");

        let published = bus.published(&out_subject);
        assert_eq!(published.len(), 1);
        let output = CommittedOutput::<E>::decode(&published[0].2).unwrap();
        match output.kind {
            OutputKind::Terminal { reason, .. } => {
                assert_eq!(reason, TerminalReason::UnsupportedCoverage);
            }
            other => panic!("expected a Terminal output, got {}", other.kind()),
        }

        assert_eq!(admission_probe.global().jobs, 0, "no job credit leaked");
        assert!(
            admission_probe.snapshot().iter().all(|r| r.jobs == 0),
            "no region credit leaked",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_state_lost_unserved_point_still_lands_its_terminal_without_looping() {
        let vehicle = 1u64;
        let partition = partition_for(vehicle);
        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        // The store holds an undecodable checkpoint at revision 7 (state lost), and the point is unserved.
        seed_checkpoint_bytes(&store, vehicle, partition, 7, b"not a checkpoint".to_vec()).await;

        publish_raw(&bus, partition, vehicle, 1_775_000_000_000_000).await;

        let catalog = Arc::new(unserved_catalog());
        let admission = admission_for(&catalog);
        let admission_probe = admission.clone();
        let report = clean_report(partition);
        let worker = build_worker_with(
            config(partition),
            catalog,
            &bus,
            &store,
            admission,
            &report,
            shutdown.clone(),
        );

        let out_subject = output_subject(u64::from(partition));
        let driver = {
            let bus = bus.clone();
            let shutdown = shutdown.clone();
            let out_subject = out_subject.clone();
            async move {
                wait_until(|| !bus.published(&out_subject).is_empty()).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Operator);
            }
        };

        let (stats, ()) = tokio::join!(worker.run(), driver);
        let stats = stats.expect("worker ran");

        assert_eq!(stats.terminal, 1, "the terminal landed");
        assert_eq!(stats.committed, 1, "exactly one commit, no loop");
        assert_eq!(stats.conflicts, 0, "the CAS targeted the stale revision");
        assert_eq!(stats.quarantined_vehicles, 0);

        let published = bus.published(&out_subject);
        assert_eq!(published.len(), 1, "exactly one terminal output");
        let output = CommittedOutput::<E>::decode(&published[0].2).unwrap();
        match output.kind {
            OutputKind::Terminal { reason, .. } => {
                assert_eq!(reason, TerminalReason::UnsupportedCoverage);
            }
            other => panic!("expected a Terminal output, got {}", other.kind()),
        }

        let (checkpoint, prepared) = store.load(VehicleId(vehicle)).await.unwrap();
        assert!(checkpoint.is_some(), "a fresh checkpoint was promoted");
        assert!(prepared.is_none(), "nothing is left staged");
        assert_eq!(bus.acked_count(&raw_subject(u64::from(partition))), 1);
        assert_eq!(admission_probe.global().jobs, 0, "no job credit leaked");
    }

    #[tokio::test(start_paused = true)]
    async fn tick_records_the_oldest_pending_gauge() {
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry_sdk::Resource;
        use opentelemetry_sdk::error::OTelSdkResult;
        use opentelemetry_sdk::metrics::data::{Gauge as GaugeData, ResourceMetrics};
        use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
        use opentelemetry_sdk::metrics::reader::MetricReader as _;
        use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};

        // A push exporter that does nothing; the test reads the gauge via `collect` directly.
        #[derive(Debug, Default)]
        struct NoopExporter;

        impl PushMetricExporter for NoopExporter {
            async fn export(&self, _metrics: &mut ResourceMetrics) -> OTelSdkResult {
                Ok(())
            }
            fn force_flush(&self) -> OTelSdkResult {
                Ok(())
            }
            fn shutdown(&self) -> OTelSdkResult {
                Ok(())
            }
            fn temporality(&self) -> Temporality {
                Temporality::Cumulative
            }
        }

        fn gauge_value(reader: &PeriodicReader<NoopExporter>, name: &str) -> Option<f64> {
            let mut rm = ResourceMetrics {
                resource: Resource::builder().build(),
                scope_metrics: Vec::new(),
            };
            reader.collect(&mut rm).expect("collect");
            for scope in &rm.scope_metrics {
                for metric in &scope.metrics {
                    if metric.name.as_ref() == name
                        && let Some(gauge) = metric.data.as_any().downcast_ref::<GaugeData<f64>>()
                    {
                        return gauge.data_points.first().map(|dp| dp.value);
                    }
                }
            }
            None
        }

        let vehicle = 1u64;
        let partition = partition_for(vehicle);
        let bus = MemoryBus::new();
        let store = MemoryCheckpointStore::new();
        let shutdown = Shutdown::new();

        let reader = PeriodicReader::builder(NoopExporter).build();
        let provider = SdkMeterProvider::builder()
            .with_reader(reader.clone())
            .with_resource(Resource::builder().with_service_name("test").build())
            .build();
        let metrics = Metrics::from_meter(provider.meter("routers_realtime"));

        publish_raw(&bus, partition, vehicle, 1_775_000_000_000_000).await;

        // A long freshness budget so no deadline pops the head; the observation keeps ageing.
        let mut worker = build_worker(
            config(partition),
            Arc::new(catalog(3_600_000)),
            &bus,
            &store,
            shutdown.clone(),
        )
        .with_metrics(metrics);

        let delivery = worker
            .raw
            .next()
            .await
            .expect("a raw delivery")
            .expect("a well-formed delivery");
        worker.on_raw(delivery).await;

        worker.on_tick().await;
        let fresh = gauge_value(&reader, "oldest_pending_age_seconds")
            .expect("the tick records the gauge even when the queue just filled");
        assert!(fresh < 1.0, "a just-queued head is near zero, got {fresh}");

        tokio::time::sleep(Duration::from_secs(5)).await;
        worker.on_tick().await;
        let aged =
            gauge_value(&reader, "oldest_pending_age_seconds").expect("the tick records the gauge");
        assert!(
            aged >= 5.0,
            "the gauge reflects the queued head's age, got {aged}",
        );
    }
}
