//! Orchestrator: the recovery coordinator (spec §3, §9, §10).
//!
//! When a partition worker (re)acquires a partition it inherits whatever the
//! previous owner left behind: prepared commits that never finished, a
//! committed frontier somewhere behind the raw journal's head, and per-vehicle
//! checkpoints that may or may not still decode. This module turns that durable
//! residue back into a running partition, crash-safely and without ever
//! re-emitting or diverging a committed history.
//!
//! # Resolve prepared commits first (spec §9, §10)
//!
//! The governing rule is *resolve any prepared commit before anything else*. A
//! [`PreparedCommit`](crate::store::checkpoint::PreparedCommit) is the durable
//! record of a commit's intent: exact output bytes plus the next checkpoint to
//! install. [`recover_partition`] walks every prepared record a partition holds
//! ([`CheckpointStore::list_prepared`]) and re-drives it through
//! [`Committer::finish_prepared`], which re-publishes the identical bytes (the
//! broker deduplicates them) and promotes the checkpoint. A prepared record
//! that was already published but not promoted is simply promoted; one that was
//! never published is published then promoted. Either way the partition emerges
//! with no half-finished commits.
//!
//! A `finish_prepared` failure — a bus outage, a store fault — does **not**
//! abort recovery. The vehicle is recorded in
//! [`RecoveryReport::prepared_failed`] and the worker keeps it in a blocked set,
//! refusing to dispatch it until a later retry of `finish_prepared` succeeds. A
//! vehicle mid-commit must never dispatch a *new* revision over an unresolved
//! one, so blocking (rather than aborting the whole partition) is the safe
//! degradation: the rest of the partition recovers and the blocked vehicles
//! drain as the dependency heals.
//!
//! # Resuming the raw journal
//!
//! Recovery reports the partition's completion `frontier`; the worker then
//! (re)creates the raw consumer starting at [`expected_start`] — the frontier's
//! successor, or the stream head when no frontier has ever been set. The raw
//! journal retains acknowledged messages ([`topology::raw`](crate::topology::raw)
//! documents why), so re-reading from `frontier + 1` replays exactly the
//! uncommitted tail and nothing already committed.
//!
//! **Consumer-start verification (a known nit, deferred to T35).** The T05
//! review flagged that `topology::raw::raw_consumer` swallows a
//! `delete_consumer` error with `let _ =` when `start` is `Some`: a transient
//! delete failure would silently rebind the *stale* durable and ignore the
//! requested start sequence, so recovery would resume from the wrong point.
//! This pure module holds no JetStream consumer handle, so it cannot inspect
//! `consumer.info()` here. Instead it exposes [`expected_start`] as the single
//! source of truth for the deliver policy the bound consumer must carry, and it
//! is the wiring in T35 (`bin/orchestrator.rs`, which owns the consumer) that
//! must, after `raw_consumer` returns, assert `consumer.info().config`'s
//! `deliver_policy` is `ByStartSequence` at `expected_start(frontier)` (or
//! `All` when no frontier exists) and refuse to run otherwise. See the
//! follow-up.
//!
//! # Suppression and reconstruction are not this module's job
//!
//! Two responsibilities that look like recovery's belong elsewhere, by design:
//!
//! * **Replay suppression.** Once the raw consumer resumes from `frontier + 1`
//!   the broker will still redeliver the uncommitted tail, some of which a crash
//!   committed but did not acknowledge. Dropping those is the *reader*'s job
//!   ([`reader`](crate::orchestrator::reader)): a sequence at or below the
//!   frontier is [`SuppressReason::BehindFrontier`](crate::orchestrator::reader),
//!   and one already folded into the loaded checkpoint is
//!   [`SuppressReason::Committed`](crate::orchestrator::reader). Recovery only
//!   positions the consumer; it suppresses nothing itself.
//!
//! * **Expected-job reconstruction.** Recovery does not persist or replay
//!   in-flight jobs. It does not need to: a job's identity is the digest of its
//!   [`JobIdentity`](crate::protocol::job::JobIdentity), so a job re-dispatched
//!   from the restored checkpoint and the same observation hashes to the *same*
//!   [`JobId`](crate::protocol::ids::JobId) as the one lost to the crash. An
//!   in-flight matcher's result therefore still validates against the
//!   re-dispatched job (T16), and the broker deduplicates the re-published job.
//!   This is reconstruction by determinism, and the unit tests pin it.
//!
//! # Lazy per-vehicle restore
//!
//! [`restore_vehicle`] is the worker's first-touch path: on a vehicle's first
//! observation after acquiring its partition it loads the durable state,
//! resolves any surviving prepared commit first (the same §9 rule), and decodes
//! the checkpoint. A checkpoint whose bytes will not decode, or that was written
//! under a superseded schema, cannot be resumed: restore reports it
//! [`Absent`](CheckpointState::Absent) with [`ResetReason::StateLost`], so the
//! next dispatch opens a new segment and the commit emits a `Reset{StateLost}`.
//! The prior segment's finalised layers are never touched — the materializer
//! keeps old segments — so state loss costs continuity, not history.

use routers_network::Entry;
use thiserror::Error;

use crate::bus::adapter::Publisher;
use crate::event::VehicleId;
use crate::orchestrator::commit::{CommitError, Committer};
use crate::orchestrator::scheduler::CheckpointState;
use crate::protocol::ids::{Revision, SCHEMA_VERSION};
use crate::protocol::output::{CommittedOutput, ResetReason};
use crate::store::checkpoint::{CheckpointStore, VehicleCheckpoint};

/// What a partition's recovery pass resolved, for the worker to act on.
///
/// The worker seeds its raw consumer from `frontier` (via [`expected_start`]),
/// counts `prepared_finished` against `prepared_found` for observability, and
/// keeps `prepared_failed` as its initial blocked set — those vehicles must not
/// dispatch until a later [`Committer::finish_prepared`] clears them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    /// The partition this pass recovered.
    pub partition: u16,
    /// The partition's completion frontier: the last raw sequence whose commit
    /// is fully durable, or `None` if none was ever recorded.
    pub frontier: Option<u64>,
    /// How many prepared commits the partition held when recovery began.
    pub prepared_found: usize,
    /// How many of those were driven to completion (published and promoted).
    pub prepared_finished: usize,
    /// The vehicles whose prepared commit could not be completed — a transient
    /// bus or store fault. The worker blocks these until a retry succeeds.
    pub prepared_failed: Vec<VehicleId>,
}

/// Why recovery could not proceed.
///
/// A failure to *complete* an individual prepared commit is not an error — it
/// is recorded in [`RecoveryReport::prepared_failed`] and the pass carries on.
/// These variants are the faults that make the pass itself unsafe to continue:
/// a store read that did not answer ([`Store`](Self::Store)), or — on the lazy
/// [`restore_vehicle`] path — a surviving prepared commit that could not be
/// resolved before the vehicle's state could be trusted
/// ([`Unfinished`](Self::Unfinished)).
#[derive(Debug, Error)]
pub enum RecoveryError<SE> {
    /// A checkpoint-store read (the frontier, the prepared list, or a vehicle
    /// load) failed. Recovery cannot proceed on a partition it cannot read.
    #[error("the checkpoint store rejected a recovery read")]
    Store(#[source] SE),
    /// A vehicle carried a surviving prepared commit that could not be
    /// completed, so its committed state cannot yet be trusted. The caller
    /// blocks the vehicle and retries.
    #[error("a surviving prepared commit could not be completed during restore")]
    Unfinished(#[source] CommitError<SE>),
}

/// The delivery start position the raw consumer must bind to after recovery:
/// the sequence *after* the completion `frontier`, or `None` (bind at the
/// stream head, delivering everything) when no frontier has been recorded.
///
/// This is the single source of truth for the deliver policy T35 must verify
/// against the consumer it creates (see the module docs): `Some(n)` demands a
/// `ByStartSequence { start_sequence: n }` policy, `None` demands `All`.
/// Saturating so a `u64::MAX` frontier cannot wrap — it would only ever pin the
/// consumer at the very end, never past the start.
#[must_use]
pub fn expected_start(frontier: Option<u64>) -> Option<u64> {
    frontier.map(|sequence| sequence.saturating_add(1))
}

/// Recover one partition: resolve every surviving prepared commit and report
/// the frontier the raw consumer should resume from.
///
/// Reads the partition's `frontier` and its prepared records, then re-drives
/// each prepared commit through [`Committer::finish_prepared`]. A completion
/// failure is recorded (the vehicle lands in
/// [`RecoveryReport::prepared_failed`]) and never aborts the pass — the store
/// keeps the prepared record for a later retry. Only a failed store *read*
/// aborts, as [`RecoveryError::Store`].
///
/// The `partition` index the store keys `list_prepared` by is best-effort (the
/// store repairs it), so a listed vehicle may already have been promoted away;
/// `finish_prepared` is idempotent and treats that as a completed no-op, so it
/// still counts as finished.
pub async fn recover_partition<E, S, P>(
    store: &S,
    committer: &Committer<E, S, P>,
    partition: u16,
) -> Result<RecoveryReport, RecoveryError<S::Error>>
where
    E: Entry + serde::de::DeserializeOwned,
    S: CheckpointStore,
    P: Publisher<CommittedOutput<E>>,
{
    let frontier = store
        .frontier(partition)
        .await
        .map_err(RecoveryError::Store)?
        .map(|f| f.sequence);

    let prepared = store
        .list_prepared(partition)
        .await
        .map_err(RecoveryError::Store)?;

    let prepared_found = prepared.len();
    let mut prepared_finished = 0;
    let mut prepared_failed = Vec::new();

    for (vehicle, record) in prepared {
        match committer.finish_prepared(vehicle, partition, record).await {
            Ok(_) => prepared_finished += 1,
            // A bus or store fault: leave the record for a retry and block the
            // vehicle rather than abandoning the whole partition.
            Err(_) => prepared_failed.push(vehicle),
        }
    }

    Ok(RecoveryReport {
        partition,
        frontier,
        prepared_found,
        prepared_finished,
        prepared_failed,
    })
}

/// The outcome of restoring one vehicle's durable state.
///
/// `checkpoint` is what the worker installs into its scheduler:
/// [`Present`](CheckpointState::Present) when a checkpoint decoded cleanly,
/// [`Absent`](CheckpointState::Absent) when none existed or it could not be
/// resumed. `reset` is [`ResetReason::StateLost`] exactly when a checkpoint
/// existed but was unusable — the next dispatch then opens a new segment.
#[derive(Clone, Debug)]
pub struct Restored<E: Entry> {
    /// The committed checkpoint the worker resumes from, or `Absent`.
    pub checkpoint: CheckpointState<E>,
    /// The reset the next dispatch must apply, if state was lost. `None` on a
    /// clean restore (present or genuinely fresh).
    pub reset: Option<ResetReason>,
    /// The revision the store still holds for this vehicle, even when the
    /// checkpoint could not be decoded. This is the base a `Reset { StateLost }`
    /// commit must compare-and-swap against: the vehicle resumes with a fresh
    /// `base: None`, but the store still carries the stale checkpoint's revision,
    /// so a commit staged with `expected_base: None` would hit
    /// [`PrepareOutcome::Conflict`](crate::store::checkpoint::PrepareOutcome::Conflict)
    /// and loop. `None` when nothing was stored (a genuinely fresh vehicle).
    pub prior: Option<Revision>,
}

/// Restore one vehicle's state on its first observation after the worker
/// acquires its partition.
///
/// Resolves any surviving prepared commit first (the §9 rule: a mid-commit
/// vehicle is completed before its committed state is read), reloading
/// afterwards so the promoted checkpoint is the one decoded. A prepared commit
/// that cannot be completed is [`RecoveryError::Unfinished`] — the worker blocks
/// the vehicle and retries.
///
/// With the prepared record resolved, the committed checkpoint is decoded:
///
/// * no checkpoint ⇒ [`Absent`](CheckpointState::Absent), no reset (a genuinely
///   fresh vehicle);
/// * a checkpoint that decodes and carries the current
///   [`SCHEMA_VERSION`](crate::protocol::ids::SCHEMA_VERSION) ⇒
///   [`Present`](CheckpointState::Present), no reset;
/// * a checkpoint whose bytes will not decode, or that was written under a
///   superseded schema ⇒ [`Absent`](CheckpointState::Absent) with
///   [`ResetReason::StateLost`]. Prior finalised layers stay untouched.
///
/// The graph snapshot a checkpoint was solved against is *not* compared here:
/// this module never learns the region's current
/// [`GraphVersion`](crate::protocol::ids::GraphVersion). That comparison is the
/// resolver's (T07) and dispatch's — a differing resolved graph produces a
/// distinct [`JobIdentity`](crate::protocol::job::JobIdentity) and the
/// reconcile there decides resumability. See the follow-up.
pub async fn restore_vehicle<E, S, P>(
    store: &S,
    vehicle: VehicleId,
    partition: u16,
    committer: &Committer<E, S, P>,
) -> Result<Restored<E>, RecoveryError<S::Error>>
where
    E: Entry + serde::de::DeserializeOwned,
    S: CheckpointStore,
    P: Publisher<CommittedOutput<E>>,
{
    let (stored, prepared) = store.load(vehicle).await.map_err(RecoveryError::Store)?;

    // Resolve a surviving prepared commit before trusting any committed state,
    // then reload so the promoted checkpoint is what we decode.
    let stored = if let Some(prepared) = prepared {
        committer
            .finish_prepared(vehicle, partition, prepared)
            .await
            .map_err(RecoveryError::Unfinished)?;
        store.load(vehicle).await.map_err(RecoveryError::Store)?.0
    } else {
        stored
    };

    let Some(stored) = stored else {
        // No committed checkpoint: a genuinely fresh vehicle, not state loss.
        return Ok(Restored {
            checkpoint: CheckpointState::Absent,
            reset: None,
            prior: None,
        });
    };

    // The store holds this revision regardless of whether the bytes decode; a
    // state-lost reset's commit must compare-and-swap against it.
    let prior = Some(stored.revision);

    match VehicleCheckpoint::<E>::decode(&stored.bytes) {
        // A clean, current-schema checkpoint resumes as-is.
        Ok(checkpoint) if checkpoint.schema == SCHEMA_VERSION => Ok(Restored {
            checkpoint: CheckpointState::Present(checkpoint),
            reset: None,
            prior,
        }),
        // Decoded but stale schema, or would not decode at all: unusable state.
        // Start a new segment; the old segment's finalised layers stay.
        Ok(_) | Err(_) => Ok(Restored {
            checkpoint: CheckpointState::Absent,
            reset: Some(ResetReason::StateLost),
            prior,
        }),
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
    use routers_transition::Matcher;
    use routers_transition::costing::{
        CostingStrategies, DefaultEmissionCost, DefaultTransitionCost,
    };
    use routers_transition::layer::generation::StandardGenerator;
    use routers_transition::matcher::{Origin, Trip};
    use routers_transition::weigh::AllCompute;

    use super::*;
    use crate::bus::Wire;
    use crate::bus::adapter::PublishError;
    use crate::bus::memory::{MemoryBus, MemoryPublisher};
    use crate::event::Payload;
    use crate::orchestrator::commit::{CommitConfig, Decision, Plan, plan};
    use crate::orchestrator::dispatch::{DispatchConfig, build_context};
    use crate::protocol::ids::{
        GraphVersion, JobId, ObservationId, RegionId, Revision, SchemaVersion, SegmentId,
    };
    use crate::protocol::job::JobIdentity;
    use crate::protocol::output::{CommittedOutput, OutputKind, TerminalReason};
    use crate::store::checkpoint::{
        CommitPhase, MemoryCheckpointStore, PartitionFrontier, PrepareOutcome, PreparedCommit,
    };
    use crate::topology::output::output_subject;

    type E = MockEntryId;
    type Costing = CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, MockEntryId>;

    const PARTITION: u16 = 7;
    const SUBJECT: &str = "events.matched.v1.p.7";
    const TRACE_START_US: i64 = 1_775_000_000_000_000;

    fn region() -> RegionId {
        RegionId::new("r1").unwrap()
    }

    fn graph() -> GraphVersion {
        GraphVersion::new("g1").unwrap()
    }

    fn obs(seq: u64) -> ObservationId {
        ObservationId {
            partition: PARTITION,
            sequence: seq,
        }
    }

    fn committer(
        store: MemoryCheckpointStore,
        bus: &MemoryBus,
    ) -> Committer<E, MemoryCheckpointStore, MemoryPublisher<CommittedOutput<E>>> {
        Committer::new(
            store,
            bus.publisher::<CommittedOutput<E>>(),
            CommitConfig {
                publish_attempts: 4,
                backoff: Duration::ZERO,
            },
        )
    }

    /// A terminal-only plan whose single output is durable and self-contained,
    /// so a commit of it installs a valid checkpoint the store can hand back.
    fn terminal_plan(seq: u64) -> Plan<E> {
        let id = JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(1),
            observation: obs(seq),
            base: None,
            graph: graph(),
            region: region(),
        };
        plan(
            None,
            Decision::Terminal {
                job: id.job_id(),
                identity: id,
                reason: TerminalReason::Unanchored,
                closes_segment: false,
                segment: SegmentId(seq),
            },
            obs(seq),
            &region(),
            &graph(),
            0,
        )
    }

    /// Stage (and leave prepared) a commit that fails on its publish, so the
    /// store holds an unpublished prepared record ready for recovery.
    async fn stage_unpublished(
        store: &MemoryCheckpointStore,
        bus: &MemoryBus,
        vehicle: VehicleId,
        seq: u64,
    ) {
        bus.fail_next_publish(PublishError::Failed(anyhow::anyhow!("bus down")));
        let c = committer(store.clone(), bus);
        c.commit(vehicle, PARTITION, terminal_plan(seq), None, obs(seq))
            .await
            .expect_err("the armed publish failure fails the commit");
        let (checkpoint, prepared) = store.load(vehicle).await.unwrap();
        assert!(checkpoint.is_none(), "nothing is committed yet");
        assert!(!prepared.unwrap().is_published(), "and it is unpublished");
    }

    /// Directly install `bytes` as a vehicle's committed checkpoint by staging a
    /// self-contained prepared record whose `next_checkpoint` is `bytes` and
    /// finishing it — the only way to seed arbitrary (including corrupt) bytes.
    async fn install_checkpoint_bytes(
        store: &MemoryCheckpointStore,
        bus: &MemoryBus,
        vehicle: VehicleId,
        seq: u64,
        bytes: Vec<u8>,
    ) {
        let output = CommittedOutput::<E>::new(
            JobId(seq as u128),
            vehicle,
            obs(seq),
            Revision(seq),
            SegmentId(seq),
            OutputKind::Terminal {
                reason: TerminalReason::Unanchored,
                closes_segment: false,
            },
        );
        let entries: Vec<(String, String, Vec<u8>)> = vec![(
            output_subject(u64::from(PARTITION)),
            output.msg_id(),
            output.encode().unwrap(),
        )];
        let prepared = PreparedCommit {
            output: output.id,
            output_subject: output_subject(u64::from(PARTITION)),
            output_bytes: postcard::to_allocvec(&entries).unwrap(),
            next_checkpoint: bytes,
            next_revision: Revision(seq),
            next_segment: SegmentId(seq),
            expected_base: None,
            phase: CommitPhase::Prepared,
            raw: obs(seq),
        };
        assert_eq!(
            store
                .prepare(vehicle, PARTITION, prepared.clone())
                .await
                .unwrap(),
            PrepareOutcome::Prepared,
        );
        committer(store.clone(), bus)
            .finish_prepared(vehicle, PARTITION, prepared)
            .await
            .expect("seed commit installs the checkpoint bytes");
    }

    fn bent_road() -> MockNetwork {
        MockNetworkBuilder::new()
            .node(1, geo::point!(x: -118.15, y: 34.15))
            .node(2, geo::point!(x: -118.16, y: 34.15))
            .node(3, geo::point!(x: -118.17, y: 34.15))
            .node(4, geo::point!(x: -118.17, y: 34.14))
            .node(5, geo::point!(x: -118.18, y: 34.14))
            .edge(1, 2)
            .edge(2, 3)
            .edge(3, 4)
            .edge(4, 5)
            .build()
    }

    fn trace_origins() -> Vec<Origin> {
        [
            geo::point!(x: -118.151, y: 34.1503),
            geo::point!(x: -118.155, y: 34.1503),
            geo::point!(x: -118.165, y: 34.1503),
            geo::point!(x: -118.170, y: 34.1490),
            geo::point!(x: -118.172, y: 34.1403),
            geo::point!(x: -118.179, y: 34.1403),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, pt)| Origin::new(pt, TRACE_START_US + i as i64 * 5_000_000))
        .collect()
    }

    /// Mint a non-empty [`Trip`] by pushing `origins` through a matcher — the
    /// only way to build one, since `push_layer` is crate-private.
    fn trip_with(origins: &[Origin]) -> Trip<E> {
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

    fn checkpoint_at(revision: u64, segment: u64, schema: SchemaVersion) -> VehicleCheckpoint<E> {
        VehicleCheckpoint {
            trip: trip_with(&trace_origins()),
            last_input: obs(revision),
            revision: Revision(revision),
            segment: SegmentId(segment),
            finalized_through: None,
            graph: graph(),
            schema,
            region: region(),
        }
    }

    fn continuing_head() -> Payload {
        let last = *trace_origins().last().unwrap();
        Payload {
            vehicle_id: VehicleId(1),
            timestamp: chrono::DateTime::from_timestamp_micros(last.timestamp + 5_000_000).unwrap(),
            point: geo::point!(x: -118.180, y: 34.1403),
        }
    }

    /// Rebuild a job's identity exactly as the dispatcher would, from a
    /// checkpoint and the head observation — the reconstruction-by-determinism
    /// path recovery relies on.
    fn identity_from(
        checkpoint: Option<&VehicleCheckpoint<E>>,
        observation: ObservationId,
        head: &Payload,
    ) -> JobIdentity {
        let built = build_context(checkpoint, observation, head, &DispatchConfig::default());
        JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(1),
            observation,
            base: built.base,
            graph: graph(),
            region: region(),
        }
    }

    // ----- expected_start (pure) -----

    #[test]
    fn expected_start_is_the_frontier_successor_or_the_head() {
        assert_eq!(expected_start(None), None);
        assert_eq!(expected_start(Some(0)), Some(1));
        assert_eq!(expected_start(Some(41)), Some(42));
        // Saturating: a maxed frontier never wraps past the start.
        assert_eq!(expected_start(Some(u64::MAX)), Some(u64::MAX));
    }

    // ----- recover_partition -----

    #[tokio::test]
    async fn recovery_reports_the_stored_frontier() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        store
            .set_frontier(PartitionFrontier {
                partition: PARTITION,
                sequence: 99,
            })
            .await
            .unwrap();

        let report = recover_partition(&store, &committer(store.clone(), &bus), PARTITION)
            .await
            .unwrap();

        assert_eq!(report.frontier, Some(99));
        assert_eq!(expected_start(report.frontier), Some(100));
        assert_eq!(report.prepared_found, 0);
    }

    #[tokio::test]
    async fn a_prepared_unpublished_record_is_published_and_promoted() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);
        stage_unpublished(&store, &bus, vehicle, 100).await;

        let report = recover_partition(&store, &committer(store.clone(), &bus), PARTITION)
            .await
            .unwrap();

        assert_eq!(report.prepared_found, 1);
        assert_eq!(report.prepared_finished, 1);
        assert!(report.prepared_failed.is_empty());
        // The output is now durable and the checkpoint is installed.
        assert_eq!(bus.published(SUBJECT).len(), 1);
        let (checkpoint, prepared) = store.load(vehicle).await.unwrap();
        assert_eq!(checkpoint.unwrap().revision, Revision(100));
        assert!(prepared.is_none(), "the prepared record is promoted away");
    }

    #[tokio::test]
    async fn a_published_unpromoted_record_is_republished_and_promoted() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);

        // A crash between publish and promote: the output is durable and the
        // record is marked published, but promotion never ran.
        store.fail_next(crate::store::checkpoint::Op::Promote);
        committer(store.clone(), &bus)
            .commit(vehicle, PARTITION, terminal_plan(100), None, obs(100))
            .await
            .expect_err("promotion fails");
        assert_eq!(bus.published(SUBJECT).len(), 1);
        assert!(store.load(vehicle).await.unwrap().1.unwrap().is_published());

        let report = recover_partition(&store, &committer(store.clone(), &bus), PARTITION)
            .await
            .unwrap();

        assert_eq!(report.prepared_finished, 1);
        // The republish deduplicates to the one already-durable copy.
        assert_eq!(bus.published(SUBJECT).len(), 1, "republish deduplicates");
        assert_eq!(
            store.load(vehicle).await.unwrap().0.unwrap().revision,
            Revision(100)
        );
        assert!(store.load(vehicle).await.unwrap().1.is_none());
    }

    #[tokio::test]
    async fn a_bus_failure_leaves_the_record_and_reports_the_vehicle_blocked() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);
        stage_unpublished(&store, &bus, vehicle, 100).await;

        // The bus is still down when recovery tries to re-drive the record.
        bus.fail_next_publish(PublishError::Failed(anyhow::anyhow!("still down")));
        let report = recover_partition(&store, &committer(store.clone(), &bus), PARTITION)
            .await
            .unwrap();

        assert_eq!(report.prepared_found, 1);
        assert_eq!(report.prepared_finished, 0);
        assert_eq!(report.prepared_failed, vec![vehicle]);
        // The record survives for a later retry; nothing was committed.
        let (checkpoint, prepared) = store.load(vehicle).await.unwrap();
        assert!(checkpoint.is_none());
        assert!(prepared.is_some(), "the prepared record is retained");
    }

    #[tokio::test]
    async fn a_read_failure_aborts_recovery() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        store.fail_next(crate::store::checkpoint::Op::Frontier);

        let err = recover_partition(&store, &committer(store.clone(), &bus), PARTITION)
            .await
            .expect_err("a failed frontier read aborts");
        assert!(matches!(err, RecoveryError::Store(_)), "got {err:?}");
    }

    // ----- restore_vehicle -----

    #[tokio::test]
    async fn restore_of_an_unknown_vehicle_is_absent_without_reset() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();

        let restored: Restored<E> = restore_vehicle(
            &store,
            VehicleId(1),
            PARTITION,
            &committer(store.clone(), &bus),
        )
        .await
        .unwrap();

        assert!(matches!(restored.checkpoint, CheckpointState::Absent));
        assert_eq!(restored.reset, None);
        assert_eq!(restored.prior, None, "nothing stored, so no prior revision");
    }

    #[tokio::test]
    async fn restore_of_a_clean_checkpoint_is_present_without_reset() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);
        let cp = checkpoint_at(100, 100, SCHEMA_VERSION);
        install_checkpoint_bytes(&store, &bus, vehicle, 100, cp.encode().unwrap()).await;

        let restored: Restored<E> =
            restore_vehicle(&store, vehicle, PARTITION, &committer(store.clone(), &bus))
                .await
                .unwrap();

        match restored.checkpoint {
            CheckpointState::Present(got) => assert_eq!(got.revision, Revision(100)),
            other => panic!("expected Present, got {other:?}"),
        }
        assert_eq!(restored.reset, None);
        assert_eq!(restored.prior, Some(Revision(100)));
    }

    #[tokio::test]
    async fn restore_of_a_corrupt_checkpoint_is_state_lost() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);
        // Bytes that are not a valid postcard `VehicleCheckpoint`.
        install_checkpoint_bytes(&store, &bus, vehicle, 100, b"not a checkpoint".to_vec()).await;

        let restored: Restored<E> =
            restore_vehicle(&store, vehicle, PARTITION, &committer(store.clone(), &bus))
                .await
                .unwrap();

        assert!(matches!(restored.checkpoint, CheckpointState::Absent));
        assert_eq!(restored.reset, Some(ResetReason::StateLost));
        assert_eq!(
            restored.prior,
            Some(Revision(100)),
            "state was lost but the store still holds the stale revision to CAS against",
        );
    }

    #[tokio::test]
    async fn restore_of_a_stale_schema_checkpoint_is_state_lost() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);
        // Decodes cleanly, but under a schema this build no longer speaks.
        let stale = checkpoint_at(100, 100, SchemaVersion(SCHEMA_VERSION.0 + 1));
        install_checkpoint_bytes(&store, &bus, vehicle, 100, stale.encode().unwrap()).await;

        let restored: Restored<E> =
            restore_vehicle(&store, vehicle, PARTITION, &committer(store.clone(), &bus))
                .await
                .unwrap();

        assert!(matches!(restored.checkpoint, CheckpointState::Absent));
        assert_eq!(restored.reset, Some(ResetReason::StateLost));
    }

    #[tokio::test]
    async fn restore_resolves_a_surviving_prepared_commit_first() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);
        // A prepared-but-unpublished commit is the vehicle's only state.
        stage_unpublished(&store, &bus, vehicle, 100).await;

        let restored: Restored<E> =
            restore_vehicle(&store, vehicle, PARTITION, &committer(store.clone(), &bus))
                .await
                .unwrap();

        // The prepared commit was finished, then the promoted checkpoint read.
        assert_eq!(bus.published(SUBJECT).len(), 1);
        match restored.checkpoint {
            CheckpointState::Present(got) => assert_eq!(got.revision, Revision(100)),
            other => panic!("expected Present after finishing the prepared commit, got {other:?}"),
        }
        assert_eq!(restored.reset, None);
        assert!(store.load(vehicle).await.unwrap().1.is_none());
    }

    #[tokio::test]
    async fn restore_blocks_when_a_prepared_commit_cannot_finish() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);
        stage_unpublished(&store, &bus, vehicle, 100).await;

        // The bus is still down: the prepared commit cannot be resolved.
        bus.fail_next_publish(PublishError::Failed(anyhow::anyhow!("still down")));
        let err = restore_vehicle(&store, vehicle, PARTITION, &committer(store.clone(), &bus))
            .await
            .expect_err("an unresolvable prepared commit blocks restore");
        assert!(matches!(err, RecoveryError::Unfinished(_)), "got {err:?}");
    }

    // ----- reconstruction by determinism (deliverable 4) -----

    #[tokio::test]
    async fn a_job_rebuilt_from_the_restored_checkpoint_keeps_its_id() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);
        let head = continuing_head();
        let observation = obs(200);

        // The job id the pre-crash dispatcher would have minted.
        let before = checkpoint_at(100, 100, SCHEMA_VERSION);
        let id_before = identity_from(Some(&before), observation, &head).job_id();

        // Round-trip the checkpoint through the store and restore it.
        install_checkpoint_bytes(&store, &bus, vehicle, 100, before.encode().unwrap()).await;
        let restored: Restored<E> =
            restore_vehicle(&store, vehicle, PARTITION, &committer(store.clone(), &bus))
                .await
                .unwrap();
        let CheckpointState::Present(after) = restored.checkpoint else {
            panic!("the checkpoint must restore");
        };

        // The re-dispatched job hashes to exactly the same id, so an in-flight
        // matcher's result still validates and the broker dedups the republish.
        let id_after = identity_from(Some(&after), observation, &head).job_id();
        assert_eq!(id_before, id_after);
    }
}
