//! Orchestrator: the recovery coordinator (spec §3, §9, §10).
//!
//! Turns the durable residue a re-acquired partition inherits back into a
//! running partition without diverging committed history. The governing rule is
//! to resolve any prepared commit first: [`recover_partition`] re-drives each
//! record through the idempotent [`Committer::finish_prepared`]; one that cannot
//! complete lands in [`RecoveryReport::prepared_failed`] and blocks its vehicle
//! rather than aborting. [`restore_vehicle`] applies the rule lazily.

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    /// The partition this pass recovered.
    pub partition: u16,
    /// The completion frontier: the last raw sequence whose commit is fully
    /// durable, or `None` if none was ever recorded.
    pub frontier: Option<u64>,
    /// How many prepared commits the partition held when recovery began.
    pub prepared_found: usize,
    /// How many of those were driven to completion (published and promoted).
    pub prepared_finished: usize,
    /// Vehicles whose prepared commit could not be completed; the worker blocks
    /// these until a retry succeeds.
    pub prepared_failed: Vec<VehicleId>,
}

/// Why recovery could not proceed. A failure to *complete* an individual
/// prepared commit is not an error (it lands in
/// [`RecoveryReport::prepared_failed`]); these are the faults that make the pass
/// itself unsafe to continue.
#[derive(Debug, Error)]
pub enum RecoveryError<SE> {
    /// A checkpoint-store read failed; recovery cannot proceed on a partition it
    /// cannot read.
    #[error("the checkpoint store rejected a recovery read")]
    Store(#[source] SE),
    /// A surviving prepared commit could not be completed, so the vehicle's
    /// committed state cannot yet be trusted. The caller blocks and retries.
    #[error("a surviving prepared commit could not be completed during restore")]
    Unfinished(#[source] CommitError<SE>),
}

/// The delivery start position the raw consumer must bind to after recovery:
/// the sequence *after* the completion `frontier`, or `None` (bind at the stream
/// head, delivering everything) when no frontier has been recorded. Saturating,
/// so a `u64::MAX` frontier cannot wrap.
#[must_use]
pub fn expected_start(frontier: Option<u64>) -> Option<u64> {
    frontier.map(|sequence| sequence.saturating_add(1))
}

/// Recover one partition: resolve every surviving prepared commit and report
/// the frontier the raw consumer should resume from.
///
/// A completion failure is recorded in [`RecoveryReport::prepared_failed`] and
/// never aborts the pass; only a failed store *read* aborts, as
/// [`RecoveryError::Store`]. `finish_prepared` is idempotent, so a vehicle
/// already promoted away still counts as finished.
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
            // Leave the record for a retry and block the vehicle.
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
#[derive(Clone, Debug)]
pub struct Restored<E: Entry> {
    /// The committed checkpoint the worker resumes from, or `Absent`.
    pub checkpoint: CheckpointState<E>,
    /// The reset the next dispatch must apply, if state was lost; `None` on a
    /// clean restore (present or genuinely fresh).
    pub reset: Option<ResetReason>,
    /// The revision the store still holds, even when the checkpoint could not be
    /// decoded: the base a `Reset { StateLost }` commit must compare-and-swap
    /// against. `None` when nothing was stored.
    pub prior: Option<Revision>,
}

/// Restore one vehicle's state on its first observation after the worker
/// acquires its partition. Resolves any surviving prepared commit first, then
/// decodes the committed checkpoint:
///
/// * no checkpoint ⇒ [`Absent`](CheckpointState::Absent), no reset;
/// * decodes at the current
///   [`SCHEMA_VERSION`] ⇒
///   [`Present`](CheckpointState::Present), no reset;
/// * will not decode, or a superseded schema ⇒
///   [`Absent`](CheckpointState::Absent) with [`ResetReason::StateLost`].
///
/// A prepared commit that cannot be completed is [`RecoveryError::Unfinished`].
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

    // Resolve a surviving prepared commit before trusting committed state.
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

    let prior = Some(stored.revision);

    match VehicleCheckpoint::<E>::decode(&stored.bytes) {
        Ok(checkpoint) if checkpoint.schema == SCHEMA_VERSION => Ok(Restored {
            checkpoint: CheckpointState::Present(checkpoint),
            reset: None,
            prior,
        }),
        // Stale schema or undecodable: unusable state, start a new segment.
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

    /// Stage a commit that fails on publish, leaving an unpublished prepared
    /// record in the store.
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

    /// Install `bytes` as a vehicle's committed checkpoint — the only way to
    /// seed arbitrary (including corrupt) bytes.
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

    /// Mint a non-empty [`Trip`] by pushing `origins` through a matcher (the
    /// only way to build one, since `push_layer` is crate-private).
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
    /// checkpoint and the head observation.
    fn identity_from(
        checkpoint: Option<&VehicleCheckpoint<E>>,
        observation: ObservationId,
        head: &Payload,
    ) -> JobIdentity {
        let built = build_context(checkpoint, observation, head, &DispatchConfig::default())
            .expect("recovery fixtures use a monotonic head");
        JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(1),
            observation,
            base: built.base,
            graph: graph(),
            region: region(),
        }
    }

    #[test]
    fn expected_start_is_the_frontier_successor_or_the_head() {
        assert_eq!(expected_start(None), None);
        assert_eq!(expected_start(Some(0)), Some(1));
        assert_eq!(expected_start(Some(41)), Some(42));
        assert_eq!(expected_start(Some(u64::MAX)), Some(u64::MAX));
    }

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

        // Crash between publish and promote: durable output, unpromoted record.
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

        // The bus is still down when recovery re-drives the record.
        bus.fail_next_publish(PublishError::Failed(anyhow::anyhow!("still down")));
        let report = recover_partition(&store, &committer(store.clone(), &bus), PARTITION)
            .await
            .unwrap();

        assert_eq!(report.prepared_found, 1);
        assert_eq!(report.prepared_finished, 0);
        assert_eq!(report.prepared_failed, vec![vehicle]);
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

    #[tokio::test]
    async fn a_job_rebuilt_from_the_restored_checkpoint_keeps_its_id() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let vehicle = VehicleId(1);
        let head = continuing_head();
        let observation = obs(200);

        let before = checkpoint_at(100, 100, SCHEMA_VERSION);
        let id_before = identity_from(Some(&before), observation, &head).job_id();

        install_checkpoint_bytes(&store, &bus, vehicle, 100, before.encode().unwrap()).await;
        let restored: Restored<E> =
            restore_vehicle(&store, vehicle, PARTITION, &committer(store.clone(), &bus))
                .await
                .unwrap();
        let CheckpointState::Present(after) = restored.checkpoint else {
            panic!("the checkpoint must restore");
        };

        let id_after = identity_from(Some(&after), observation, &head).job_id();
        assert_eq!(id_before, id_after);
    }
}
