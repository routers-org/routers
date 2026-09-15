//! Orchestrator: the partition worker loop (spec §3, §8, §9).
//!
//! Everything the other ten orchestrator modules do — read raw, schedule,
//! admit, dispatch, validate results, commit, track the frontier, arm and fire
//! deadlines, recover — is *state and pure logic*. This module is the single
//! task that owns that state for one partition and drives those pieces over the
//! [`bus::adapter`](crate::bus::adapter) traits and the
//! [`CheckpointStore`]. It holds the partition's [`Scheduler`] (its
//! `HashMap<VehicleId, VehicleState>`), its [`FrontierTracker`], and its
//! [`Deadlines`] heap, and `select!`s over four inputs — raw deliveries, result
//! deliveries, deadline ticks, and a periodic housekeeping tick — plus the
//! shutdown signal. There is no cross-partition sharing: one worker, one
//! partition, one task.
//!
//! # Ordering guarantees
//!
//! * **One active job per vehicle.** The scheduler enforces a single logical
//!   [`ActiveJob`](crate::orchestrator::scheduler::ActiveJob) per vehicle; the
//!   worker never dispatches a second while one is in flight.
//! * **Per-vehicle FIFO.** A vehicle's observations are solved in the raw
//!   sequence order the reader queued them; the head is not popped until its
//!   commit (or terminal) is durable.
//! * **Sequential commits within the partition.** Commits run *inline* on this
//!   one task, so they never overlap. The deadline path and the result path both
//!   funnel through [`PartitionWorker::commit_decision`]; the scheduler's
//!   `committing` flag plus the store's [`PrepareOutcome::Busy`](crate::store::checkpoint::PrepareOutcome::Busy)
//!   arbitration make the first to prepare win, so a real result always beats a
//!   later timeout for the same observation. (A future refinement could overlap
//!   commits *across* vehicles with a `JoinSet`; because the bus adapter futures
//!   are deliberately not `Send`, that is left as a follow-up and not built
//!   here.)
//!
//! # A note on the adapter seam
//!
//! `bus::adapter` futures are not `Send`, so the worker keeps every source,
//! publisher, and ack on this single task — it never spawns them. The raw plane
//! arrives as undecoded [`RawBytes`] alongside the delivery's `HeaderMap`, so
//! the schema header travels with each raw message: the worker hands those
//! headers to the reader, which poisons a stamped `x-routers-schema` mismatch
//! and treats an empty map as "no schema stamped".

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
///
/// The nested configs are handed to the modules the worker composes; the four
/// [`Duration`] knobs pace the loop itself. `dispatch` and `commit` mirror the
/// configs the already-built [`Dispatcher`]/[`Committer`] were constructed with,
/// so a caller that wants the whole worker described by one value has it.
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
/// the metrics layer. Every counter is a bounded, label-free number.
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
    /// Vehicles retired to the quarantine set after a permanent commit fault
    /// (an encode/decode error that would spin on retry); never dispatched
    /// again this process lifetime.
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

/// The per-vehicle provenance the worker keeps for the job currently in flight,
/// so a commit (real result or deadline terminal) has the region/graph/segment
/// and compare-and-swap base it needs without re-resolving or re-deriving them.
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
///
/// It carries the stored revision alongside the reason so a `Reset { StateLost }`
/// commit compare-and-swaps against the *stale* checkpoint the store still holds
/// (the vehicle resumes with `base: None`, but the store is not empty). Without
/// this the prepare would see a `Conflict` and the vehicle would loop.
#[derive(Clone, Copy)]
struct PendingReset {
    /// Why continuity was broken (only ever [`ResetReason::StateLost`] today —
    /// gap/teleport resets are detected at dispatch with a live checkpoint).
    reason: ResetReason,
    /// The revision the reset's commit must compare-and-swap against, or `None`
    /// when the store held nothing.
    prior: Option<Revision>,
}

/// The worker's own owned projection of [`Verdict`], taken so the immutable
/// borrow of the vehicle state ends before the worker mutates itself to act on
/// the decision (commit / park / ack / retain).
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
    /// Drop the borrow the pure validator returns, keeping only the class the
    /// worker acts on. The `Accept` job reference is not needed here — the worker
    /// commits from its own [`ActiveMeta`] provenance, not the borrowed job.
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
///
/// Generic over the network entry type `E`, the [`CheckpointStore`] `S`, the two
/// publishers (`JP` for jobs, `OP` for committed output), and the two sources
/// (`RS` for the raw plane as [`RawBytes`], `XS` for results). Production wires
/// JetStream and Valkey; tests (and T31's harness) drive the memory bus and
/// store.
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
    /// Vehicles whose commit is stuck (a store `Busy`/`Conflict`, a publish
    /// fault, or a recovery-time prepared commit that could not finish) mapped
    /// to the next instant to retry them.
    blocked: HashMap<VehicleId, Instant>,
    /// Vehicles whose last dispatch was admission-held, keeping a [`Waiting`]
    /// demand guard alive until they dispatch. (The scheduler has no field to
    /// park this on, so the worker owns it — see the module report.)
    held: HashMap<VehicleId, Waiting>,
    /// Per-vehicle provenance for the job currently in flight.
    active_meta: HashMap<VehicleId, ActiveMeta>,
    /// A pending `Reset { StateLost }` a restore reported, applied to the first
    /// commit after re-acquiring the vehicle, with the stale revision its commit
    /// must compare-and-swap against.
    pending_reset: HashMap<VehicleId, PendingReset>,
    /// Vehicles retired after a permanent commit fault (an encode/decode error a
    /// retry would only reproduce): never restored, dispatched, or retried again
    /// this process lifetime. Bounded by the number of distinct poison commits.
    quarantined: HashSet<VehicleId>,
    shutdown: Shutdown,
    drain: Drain,
    last_evict: Instant,
    stats: WorkerStats,
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
    /// The caller (T35 / the harness) has run
    /// [`recover_partition`](crate::orchestrator::recovery::recover_partition)
    /// and created `raw` starting at `report.frontier + 1`. The worker seeds its
    /// [`FrontierTracker`] from `report.frontier` and marks
    /// `report.prepared_failed` as blocked so those vehicles never dispatch a new
    /// revision until their surviving prepared commit is finished.
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
        }
    }

    /// Run the partition until shutdown, returning the run's [`WorkerStats`].
    ///
    /// One biased `select!` loop: shutdown wins, then a raw delivery, then a
    /// result delivery, then the soonest armed deadline, then the housekeeping
    /// tick. On shutdown it stops consuming, quiesces any in-flight commit
    /// (immediate, since commits run inline), persists the frontier once more,
    /// and returns. Prepared-but-unpublished records are left for recovery.
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

        // Commits run inline, so nothing is truly outstanding; the quiesce is a
        // formality that returns immediately.
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
            }
            RawDisposition::Poison { handle, reason } => {
                self.stats.poison += 1;
                debug!(reason = reason.label(), "poison raw message");
                let seq = handle.sequence();
                let _ = handle.ack().await;
                self.tracker.complete(seq);
            }
            RawDisposition::Suppressed { handle, reason } => {
                self.stats.suppressed += 1;
                if reason == SuppressReason::Coalesced {
                    self.stats.coalesced += 1;
                }
                let seq = handle.sequence();
                let _ = handle.ack().await;
                // A coalesced duplicate is not terminal: the in-flight original
                // owns its sequence's completion.
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
                    // No provenance for an accepted result should be impossible;
                    // drop defensively rather than commit with a guessed base.
                    if let Some(handle) = handle {
                        let _ = handle.ack().await;
                    }
                    return;
                };
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
                self.scheduler.park(vehicle, result, now);
                // Ack the parked delivery immediately: the content is retained in
                // memory, so a crash loses it, and redelivery / re-dispatch cover
                // that. This trades durability of an *early* answer for a simpler
                // ack model.
                if let Some(handle) = handle {
                    let _ = handle.ack().await;
                }
            }
            ResultVerdict::Reject(reason) => {
                self.stats.rejected += 1;
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
            // Reuse the pure deadline arbitration over the real vehicle state, so
            // the `committing` flag it reads is the scheduler's own — an untracked
            // vehicle has no live job to terminate, so its timer is ignored.
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
        // A quarantined vehicle carries a poison commit that a retry only
        // reproduces; it never dispatches again this process lifetime.
        if self.quarantined.contains(&vehicle) {
            return DispatchOutcome::Skipped;
        }
        if self.blocked.contains_key(&vehicle) {
            return DispatchOutcome::Skipped;
        }
        // Eligible = has a head and no active job (a committing vehicle always
        // has an active job, so this also excludes it).
        if self.scheduler.active(vehicle).is_some() || self.scheduler.head(vehicle).is_none() {
            return DispatchOutcome::Skipped;
        }

        let now = Instant::now();

        // Lazily restore the vehicle on its first touch after acquiring the
        // partition (one store read per acquisition).
        if !self.scheduler.checkpoint(vehicle).is_loaded() {
            match restore_vehicle(&self.store, vehicle, self.cfg.partition, &self.committer).await {
                Ok(restored) => {
                    if let Some(reason) = restored.reset {
                        // Carry the stored revision so the reset's commit CASes
                        // against the stale checkpoint the store still holds.
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
                    // A surviving prepared commit could not finish, or a store
                    // read failed: block and retry rather than dispatch over
                    // unresolved state.
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
                // Prefer the job's own base (a normal resume); fall back to the
                // restore's stored `prior` so a state-lost reset's commit CASes
                // against the stale revision instead of `None` (which would
                // Conflict and loop). A gap/teleport reset already carries the
                // live checkpoint's base, so the fallback only bites on state loss.
                let expected_base = d
                    .job
                    .identity
                    .base
                    .map(|b| b.revision)
                    .or_else(|| pending.and_then(|p| p.prior));
                let fire = self.cfg.deadline.fire_at(d.job.deadline);
                let job_id = d.job.id;
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
                    // The eligibility check above rules this out; guard anyway.
                    self.active_meta.remove(&vehicle);
                    return DispatchOutcome::Skipped;
                }
                self.deadlines.arm(fire, vehicle, job_id);
                self.held.remove(&vehicle);
                self.stats.dispatched += 1;

                // A just-dispatched job may already have a parked answer.
                for result in self.scheduler.take_parked(vehicle) {
                    self.on_result_inner(result, None).await;
                }
                DispatchOutcome::Done
            }
            Err(DispatchError::Held(reason)) => {
                self.stats.held += 1;
                debug!(vehicle = vehicle.0, reason = %reason, "dispatch held by admission");
                self.held
                    .insert(vehicle, self.admission.hold(&resolution.region));
                DispatchOutcome::Held
            }
            Err(error) => {
                // A publish/encode/unknown-region fault: leave the vehicle state
                // untouched (the identical job re-dispatches later) and stop
                // draining this round.
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

        // Arbitration point: the first to prepare wins. A vehicle already
        // committing (a prepared commit beat this) drops the attempt.
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

        let plan = commit::plan(
            prev.as_ref(),
            decision,
            raw,
            &meta.region,
            &meta.graph,
            now_us,
        );
        let next = plan.next.clone();

        match self
            .committer
            .commit(vehicle, self.cfg.partition, plan, meta.expected_base, raw)
            .await
        {
            Ok(_committed) => {
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
                if is_terminal {
                    self.stats.terminal += 1;
                }
                if reset_opt.is_some() {
                    self.stats.resets += 1;
                }
                self.stats.frontier = self.tracker.frontier();
            }
            Err(CommitError::Conflict { actual }) => {
                // A concurrent decision won the race. Reload the checkpoint and
                // block the vehicle; the tick retry drains it (the committing
                // flag can only be cleared by finishing, so recovery — not an
                // in-line re-dispatch — resolves it).
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
                // An encode/decode fault is deterministic: retrying only
                // reproduces it. Quarantine the vehicle rather than spin.
                self.quarantine_vehicle(vehicle, &error);
            }
            Err(error) => {
                // Busy (a prepared commit exists) or a publish/store fault: the
                // prepared record survives. Block and retry `finish_prepared`.
                warn!(vehicle = vehicle.0, %error, "commit incomplete; blocking vehicle");
                self.blocked.insert(vehicle, now + self.cfg.blocked_retry);
            }
        }
    }

    /// Retire a vehicle whose commit failed with a permanent encode/decode fault.
    ///
    /// Such a fault is deterministic — the same prepared bytes will not decode,
    /// the same output will not encode — so blocked-retry would loop forever. The
    /// vehicle is logged at error, dropped from the blocked and pending sets, and
    /// added to the quarantine set so it is never dispatched or retried again
    /// this process lifetime. Its head observation is left un-acked: the worker
    /// cannot make it durable, and advancing the frontier past it would lose it.
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
    /// There is no job to dispatch, but the scheduler can only advance past a
    /// head through a live [`ActiveJob`], so the worker reserves a zero-byte
    /// permit and activates a synthetic job first, then commits the terminal
    /// through the ordinary path.
    ///
    /// An unserved point resolves to no region, so the synthetic job has none of
    /// its own to bill. A vehicle with a committed checkpoint is billed to that
    /// checkpoint's region (keeping its provenance coherent); a fresh, unserved
    /// vehicle has no region at all and falls back to the **first catalog region**
    /// as a documented, arbitrary billing choice (the zero-byte permit is a
    /// bookkeeping placeholder, never a real solve). Either way the permit is
    /// released on every path: an early return before `try_admit` reserves
    /// nothing, an `activate` failure drops the job (and with it the permit), and
    /// a successful commit releases it when the finished job is dropped.
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
        // When the checkpoint is absent because the vehicle was state-lost, the
        // store still holds the stale revision a restore reported. Fall the
        // expected base back to that `prior` (mirroring `try_dispatch`) so the
        // terminal's CAS targets the stale revision and actually lands, instead
        // of `None` — which would `Conflict` and leave the vehicle blocked with
        // the `UnsupportedCoverage` terminal silently dropped.
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
        // Consume the state-lost reset: its `prior` has now been folded into the
        // terminal's CAS above, so leaving it would mis-target a later commit
        // against a revision this terminal is about to supersede.
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
                            // The prepared bytes will never decode: quarantine
                            // rather than retry the same poison record forever.
                            self.quarantine_vehicle(vehicle, &error);
                        }
                        Err(error) => {
                            debug!(vehicle = vehicle.0, %error, "blocked commit still cannot finish");
                            self.blocked.insert(vehicle, now + self.cfg.blocked_retry);
                        }
                    }
                }
                // No prepared record: either a conflict staged nothing, or the
                // commit was already promoted. Advance past the head so the
                // vehicle stops stalling.
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
    /// [`validate::validate`](crate::orchestrator::validate::validate) over the
    /// vehicle's real state, then project its borrow-carrying [`Verdict`] into the
    /// worker's owned [`ResultVerdict`] so the borrow ends before the worker acts.
    ///
    /// The `committing` flag the validator reads is the scheduler's own, not a
    /// proxy through the blocked set. An untracked vehicle has no state yet, so it
    /// is validated against a detached empty state: the envelope checks still run,
    /// and a well-formed early result parks (a no-op for an untracked vehicle).
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

/// Whether a commit failed permanently — an encode or decode fault a retry would
/// only reproduce. Such a vehicle is quarantined rather than blocked-retried.
fn is_permanent_commit_error<SE>(error: &CommitError<SE>) -> bool {
    matches!(error, CommitError::Decode(_) | CommitError::Encode(_))
}

/// A detached, empty [`VehicleState`] used to validate a result for a vehicle the
/// scheduler is not yet tracking (a redelivery that outran raw replay). It has no
/// checkpoint, job, or commit in flight, so validation runs only the envelope
/// checks and then parks — exactly what an untracked vehicle warrants.
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

/// The `(reset, segment)` a decision carries, read without consuming it so the
/// worker can assert the segment invariant before [`commit::plan`] takes it.
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

/// Sleep until `at`, or forever when no deadline is armed — the `select!` arm's
/// timer that only wakes the loop exactly when the next deadline is due.
async fn wait_deadline(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => core::future::pending::<()>().await,
    }
}

/// The current wall clock as absolute unix microseconds (the units the wire and
/// matcher deadline against), saturating rather than wrapping.
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

    /// A one-region catalog serving the fixture cell, with a tunable freshness
    /// budget so the deadline test can make jobs expire quickly.
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

    /// An admission controller knowing exactly the catalog's regions.
    fn admission_for(catalog: &Catalog) -> Admission {
        Admission::new(
            AdmissionConfig::default(),
            catalog.regions.iter().map(|r| &r.id),
        )
    }

    /// A recovery report for a clean partition (no frontier, nothing prepared).
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

    /// Build a worker with a caller-supplied admission controller and recovery
    /// report, so tests can inspect admission usage or seed a blocked set.
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

    /// The partition the fixture vehicle hashes to.
    fn partition_for(vehicle: u64) -> u16 {
        partition_of(VehicleId(vehicle)) as u16
    }

    /// A second vehicle id that lands in the same partition as `vehicle`.
    fn same_partition_as(vehicle: u64) -> u64 {
        let want = partition_for(vehicle);
        (vehicle + 1..)
            .find(|&v| partition_for(v) == want)
            .expect("another vehicle in the partition")
    }

    /// Publish one raw observation, returning its assigned stream sequence.
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

    /// A scripted matcher: every job gets a `Solved` result with an empty diff
    /// (enough to drive a real commit without a network), acked and published.
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

    /// Poll `cond` until it holds (or panic), advancing paused time by parking.
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

        // Three observations, published first so their sequences are 1, 2, 3.
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

        // Checkpoint revision is the last raw sequence, and every raw was acked.
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

        // Interleave the two vehicles' observations on the shared partition.
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
        // Each vehicle's checkpoint advanced to its own last observation.
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

        // No matcher runs, so the job is never answered; a short budget makes it
        // expire quickly under paused time.
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
                // The deadline fires and commits a terminal output.
                wait_until(|| !bus.published(&out_subject).is_empty()).await;
                // A real result finally straggles in for the decided observation.
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

        // No matcher: the observation dispatches and stays in flight, so a
        // redelivery coalesces against the still-outstanding original.
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
                // Wait for the original to dispatch, then force a redelivery.
                wait_until(|| !bus.published(job_filter).is_empty()).await;
                bus.redeliver_unacked(&raw_sub);
                // The coalesced duplicate is acked (the original stays in flight).
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
        // Every commit promoted; nothing is left staged for recovery.
        assert!(
            store.snapshot().prepared.is_empty(),
            "no prepared records remain"
        );
    }

    /// Install a committed checkpoint whose stored `bytes` are `garbage` at
    /// revision `rev`, by staging a self-contained prepared record and finishing
    /// it on a throwaway bus (so its output never lands on the worker's bus). The
    /// store then holds a `StoredCheckpoint` at `rev` whose bytes will not decode.
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

        // The store holds an undecodable checkpoint at revision 5: state was lost,
        // but a `Reset { StateLost }` committed with `expected_base: None` would
        // Conflict against that revision and loop. The fix CASes against it.
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
                // A reset then a match: two outputs, and no retry loop.
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

        // The two outputs, in order: a StateLost reset then the match.
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

        // The checkpoint was promoted (no prepared record survives) and the raw
        // input was acked, so the vehicle made real progress rather than spinning.
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

        // Plant a prepared record whose staged output bytes will never decode.
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

        // Start with the vehicle blocked (recovery could not finish its commit),
        // so the housekeeping tick drives `finish_prepared` and hits the decode.
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
                // Let several housekeeping ticks fire; a spinning retry would keep
                // re-blocking, a quarantine fires exactly once.
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
        // The poison record is retained (the worker stopped retrying it rather
        // than deleting or spinning on it).
        assert!(
            store.snapshot().prepared.contains_key(&VehicleId(vehicle)),
            "the undecodable record is left in place"
        );
    }

    /// A one-region catalog that serves a cell far from `point()`, so `point()`
    /// resolves to no region (an unserved coverage hole).
    fn unserved_catalog() -> Catalog {
        // The precision-4 cell of the null island — a valid geohash nowhere near
        // Sydney, so `point()` never matches this coverage.
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
        // A clone to inspect the controller after the worker has consumed its own.
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
                // No matcher runs: the terminal flows straight through the commit
                // path from the unserved resolution.
                wait_until(|| !bus.published(&out_subject).is_empty()).await;
                shutdown.trigger(crate::lifecycle::DrainReason::Operator);
            }
        };

        let (stats, ()) = tokio::join!(worker.run(), driver);
        let stats = stats.expect("worker ran");

        assert_eq!(stats.terminal, 1, "one terminal was committed");
        assert_eq!(stats.committed, 1);
        assert_eq!(stats.dispatched, 0, "no real job was dispatched");

        // Exactly one Terminal output, tagged UnsupportedCoverage.
        let published = bus.published(&out_subject);
        assert_eq!(published.len(), 1);
        let output = CommittedOutput::<E>::decode(&published[0].2).unwrap();
        match output.kind {
            OutputKind::Terminal { reason, .. } => {
                assert_eq!(reason, TerminalReason::UnsupportedCoverage);
            }
            other => panic!("expected a Terminal output, got {}", other.kind()),
        }

        // The synthetic zero-byte permit was released on the commit path: no
        // credit leaked in the billed region or globally.
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

        // The store holds an undecodable checkpoint at revision 7 (state lost),
        // and the vehicle's first observation resolves to an unserved cell. The
        // unserved terminal is committed with `expected_base: None` unless it
        // folds in the stale revision — so before the fix it would `Conflict`
        // against revision 7 and the `UnsupportedCoverage` terminal would be
        // silently dropped while the vehicle looped under blocked-retry.
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
                // No matcher runs: the terminal flows straight through the commit
                // path from the unserved resolution.
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

        // Exactly one Terminal output, tagged UnsupportedCoverage — no Conflict
        // swallowed it.
        let published = bus.published(&out_subject);
        assert_eq!(published.len(), 1, "exactly one terminal output");
        let output = CommittedOutput::<E>::decode(&published[0].2).unwrap();
        match output.kind {
            OutputKind::Terminal { reason, .. } => {
                assert_eq!(reason, TerminalReason::UnsupportedCoverage);
            }
            other => panic!("expected a Terminal output, got {}", other.kind()),
        }

        // A fresh checkpoint was promoted (no prepared record survives) and the
        // raw input was acked: real progress rather than a spin.
        let (checkpoint, prepared) = store.load(VehicleId(vehicle)).await.unwrap();
        assert!(checkpoint.is_some(), "a fresh checkpoint was promoted");
        assert!(prepared.is_none(), "nothing is left staged");
        assert_eq!(bus.acked_count(&raw_subject(u64::from(partition))), 1);
        assert_eq!(admission_probe.global().jobs, 0, "no job credit leaked");
    }
}
