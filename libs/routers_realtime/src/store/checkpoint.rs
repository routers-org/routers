//! Checkpoint records and the [`CheckpointStore`] trait, plus an in-memory
//! implementation for tests.
//!
//! A vehicle's durable state is a committed [`VehicleCheckpoint`] plus an
//! optional in-flight [`PreparedCommit`], kept apart so a commit is crash-safe:
//! prepare, publish, then promote, each step idempotent. The store never
//! decodes a checkpoint — the [`Entry`]-typed bytes are opaque — so the revision
//! and segment travel outside them in [`StoredCheckpoint`] for base compares.

use alloc::sync::Arc;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use routers_network::Entry;
use routers_transition::matcher::Trip;

use crate::event::VehicleId;
use crate::protocol::ids::{
    GraphVersion, ObservationId, OutputId, RegionId, Revision, SchemaVersion, SegmentId,
};

/// The committed resume state for one vehicle: what a matcher needs to carry
/// its match on from the last decision, plus staleness provenance. Generic over
/// the network [`Entry`] type because it carries a live [`Trip`]; the store only
/// ever sees the [`encode`](Self::encode)d bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct VehicleCheckpoint<E: Entry> {
    /// The retained trip (origins, candidates, trellis) the next solve resumes.
    pub trip: Trip<E>,
    /// The last observation folded into this checkpoint.
    pub last_input: ObservationId,
    /// The revision this checkpoint was committed at; the next commit must
    /// resume from exactly this revision or it has lost a race.
    pub revision: Revision,
    /// The continuity segment this checkpoint belongs to.
    pub segment: SegmentId,
    /// Finality watermark: layers at or below this timestamp must never be
    /// rewritten by a later revision.
    pub finalized_through: Option<i64>,
    /// The graph snapshot the retained trip was solved against.
    pub graph: GraphVersion,
    /// The wire schema this record was written under.
    pub schema: SchemaVersion,
    /// The region that owns the vehicle's solves.
    pub region: RegionId,
}

impl<E: Entry> VehicleCheckpoint<E> {
    /// Serialise to the opaque bytes the store persists (postcard).
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Decode a checkpoint the store handed back as opaque bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, postcard::Error>
    where
        E: serde::de::DeserializeOwned,
    {
        postcard::from_bytes(bytes)
    }

    /// Bundle the encoded bytes with the `revision` and `segment` the store
    /// needs out-of-band, since it cannot decode `E` to read them.
    pub fn to_stored(&self) -> Result<StoredCheckpoint, postcard::Error> {
        Ok(StoredCheckpoint {
            revision: self.revision,
            segment: self.segment,
            bytes: self.encode()?,
        })
    }
}

/// A committed checkpoint as the store holds it: the opaque
/// [`VehicleCheckpoint`] bytes, with `revision` and `segment` lifted out so the
/// store can compare bases without decoding the sealed [`Entry`] type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCheckpoint {
    /// The revision the bytes were committed at; the compare key for a prepare.
    pub revision: Revision,
    /// The continuity segment the bytes belong to.
    pub segment: SegmentId,
    /// The encoded [`VehicleCheckpoint`]; opaque to the store.
    pub bytes: Vec<u8>,
}

/// How far a [`PreparedCommit`] has progressed through the publish step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommitPhase {
    /// Staged, but its output is not yet durable on the broker.
    Prepared,
    /// The output has been accepted by the broker; only promotion remains.
    Published,
}

/// A staged commit: the exact output bytes to (re)publish and the next
/// checkpoint to install once that publish is durable. Retained with no expiry
/// so a crash mid-commit is always recoverable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedCommit {
    /// The committed output's identity and broker dedup key; two calls carrying
    /// the same `output` are the same logical commit.
    pub output: OutputId,
    /// The subject the output bytes publish on.
    pub output_subject: String,
    /// The exact bytes to publish — byte-for-byte identical on every retry so
    /// the broker dedups a re-publish rather than emitting a second output.
    pub output_bytes: Vec<u8>,
    /// The encoded next [`VehicleCheckpoint`]; installed verbatim by promote.
    pub next_checkpoint: Vec<u8>,
    /// The revision the promoted checkpoint will carry, lifted out of the bytes.
    pub next_revision: Revision,
    /// The segment the promoted checkpoint will carry.
    pub next_segment: SegmentId,
    /// The revision this commit expected to supersede; the compare guard for
    /// [`prepare`](CheckpointStore::prepare). `None` means no checkpoint may
    /// exist yet (a fresh vehicle).
    pub expected_base: Option<Revision>,
    /// How far this commit has progressed (see [`CommitPhase`]).
    pub phase: CommitPhase,
    /// The raw observation that triggered this commit.
    pub raw: ObservationId,
}

impl PreparedCommit {
    /// Whether the output is already durable on the broker.
    pub fn is_published(&self) -> bool {
        matches!(self.phase, CommitPhase::Published)
    }
}

/// The high-water mark of a partition's raw journal: the last raw sequence
/// whose commit is fully durable. Recovery resumes from `sequence + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PartitionFrontier {
    /// The partition this frontier belongs to.
    pub partition: u16,
    /// The last durably-committed raw sequence in that partition.
    pub sequence: u64,
}

/// The result of a [`prepare`](CheckpointStore::prepare) attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrepareOutcome {
    /// The record was staged: the compare matched and no prepared record existed.
    Prepared,
    /// A prepared record with the same output already exists; a safe no-op retry.
    AlreadyPrepared,
    /// The stored revision did not match `expected_base`; another commit won.
    /// `actual` is the revision stored, or `None` if no checkpoint exists.
    Conflict { actual: Option<Revision> },
    /// A prepared record for a different output is staged; the vehicle is
    /// mid-commit and must be drained before a new commit can begin.
    Busy { pending: OutputId },
}

/// The durable home of every vehicle's committed checkpoint and in-flight
/// prepared commit, plus each partition's raw-journal frontier. Persists opaque
/// bytes and small records, guaranteeing the atomicity each method documents.
#[allow(async_fn_in_trait)]
pub trait CheckpointStore: Clone + Send + Sync + 'static {
    /// The failure a store operation can report.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Load a vehicle's committed checkpoint and in-flight prepared commit, if
    /// any. A single point-in-time read, so the two halves are consistent.
    async fn load(
        &self,
        vehicle: VehicleId,
    ) -> Result<(Option<StoredCheckpoint>, Option<PreparedCommit>), Self::Error>;

    /// Compare-and-stage a commit for `vehicle` in `partition`, atomically over
    /// the vehicle's keys. An existing prepared record short-circuits to
    /// [`AlreadyPrepared`](PrepareOutcome::AlreadyPrepared) or
    /// [`Busy`](PrepareOutcome::Busy); otherwise the stored revision is compared
    /// against `expected_base` (`None` demands no checkpoint), yielding
    /// [`Conflict`](PrepareOutcome::Conflict) or a staged
    /// [`Prepared`](PrepareOutcome::Prepared).
    async fn prepare(
        &self,
        vehicle: VehicleId,
        partition: u16,
        prepared: PreparedCommit,
    ) -> Result<PrepareOutcome, Self::Error>;

    /// Mark the vehicle's prepared commit as [`CommitPhase::Published`].
    /// Idempotent for the same `output`; errors on a different staged output;
    /// succeeds as a no-op when already promoted away.
    async fn mark_published(&self, vehicle: VehicleId, output: OutputId)
    -> Result<(), Self::Error>;

    /// Promote the vehicle's prepared commit in `partition`, atomically:
    /// install the [`StoredCheckpoint`] from the prepared record, delete that
    /// record, and drop the vehicle from the partition index. Idempotent when
    /// already promoted; errors on a different staged output.
    async fn promote(
        &self,
        vehicle: VehicleId,
        partition: u16,
        output: OutputId,
    ) -> Result<(), Self::Error>;

    /// Every staged prepared commit in `partition`, so recovery can re-drive
    /// them. The index is best-effort: a listed vehicle may have been promoted
    /// away, and the store repairs such stale entries.
    async fn list_prepared(
        &self,
        partition: u16,
    ) -> Result<Vec<(VehicleId, PreparedCommit)>, Self::Error>;

    /// The partition's raw-journal frontier, or `None` if none has been set.
    async fn frontier(&self, partition: u16) -> Result<Option<PartitionFrontier>, Self::Error>;

    /// Set (overwrite) a partition's frontier. The caller advances it
    /// monotonically; the store records what it is given.
    async fn set_frontier(&self, frontier: PartitionFrontier) -> Result<(), Self::Error>;

    /// Drop a vehicle's committed checkpoint because it has gone idle; never
    /// touches a prepared record, so a commit in flight can still complete.
    async fn expire_idle(&self, vehicle: VehicleId) -> Result<(), Self::Error>;
}

/// Which [`MemoryCheckpointStore`] operation a fault has been armed against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Load,
    Prepare,
    MarkPublished,
    Promote,
    ListPrepared,
    Frontier,
    SetFrontier,
    Expire,
}

/// The error a [`MemoryCheckpointStore`] can report.
#[derive(Debug, Error)]
pub enum MemoryError {
    /// A fault armed by [`MemoryCheckpointStore::fail_next`] fired.
    #[error("injected memory-store fault")]
    Injected,
    /// An operation named an output that disagreed with the staged record.
    #[error("prepared output mismatch: staged {staged}, got {got}")]
    OutputMismatch { staged: OutputId, got: OutputId },
}

/// The mutex-guarded state behind a [`MemoryCheckpointStore`].
#[derive(Default)]
struct Inner {
    checkpoints: HashMap<VehicleId, StoredCheckpoint>,
    prepared: HashMap<VehicleId, PreparedCommit>,
    index: HashMap<u16, HashSet<VehicleId>>,
    frontiers: HashMap<u16, PartitionFrontier>,
    faults: HashSet<Op>,
}

/// An in-memory [`CheckpointStore`] for tests: a single mutex standing in for
/// the Valkey key-group atomicity, with knobs to inject faults and simulate
/// state loss. Every clone shares one backing store.
#[derive(Clone, Default)]
pub struct MemoryCheckpointStore {
    inner: Arc<Mutex<Inner>>,
}

/// A deep copy of a [`MemoryCheckpointStore`]'s state, taken for assertions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemorySnapshot {
    /// Every committed checkpoint, by vehicle.
    pub checkpoints: HashMap<VehicleId, StoredCheckpoint>,
    /// Every staged prepared commit, by vehicle.
    pub prepared: HashMap<VehicleId, PreparedCommit>,
    /// The prepared index: which vehicles are staged in each partition.
    pub index: HashMap<u16, HashSet<VehicleId>>,
    /// Every partition frontier.
    pub frontiers: HashMap<u16, PartitionFrontier>,
}

impl MemoryCheckpointStore {
    /// A fresh, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm a one-shot fault: the next call of `op` returns
    /// [`MemoryError::Injected`], then disarms.
    pub fn fail_next(&self, op: Op) {
        self.lock().faults.insert(op);
    }

    /// A deep copy of the store's maps, for assertions.
    pub fn snapshot(&self) -> MemorySnapshot {
        let inner = self.lock();
        MemorySnapshot {
            checkpoints: inner.checkpoints.clone(),
            prepared: inner.prepared.clone(),
            index: inner.index.clone(),
            frontiers: inner.frontiers.clone(),
        }
    }

    /// Drop every committed checkpoint while leaving prepared records intact, to
    /// simulate the "checkpoint missing but prepared exists" state loss.
    pub fn drop_all_checkpoints(&self) {
        self.lock().checkpoints.clear();
    }

    /// Drop one vehicle's prepared record and its index entry.
    pub fn drop_prepared(&self, vehicle: VehicleId) {
        let mut inner = self.lock();
        inner.prepared.remove(&vehicle);
        for set in inner.index.values_mut() {
            set.remove(&vehicle);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("checkpoint store mutex poisoned")
    }
}

impl CheckpointStore for MemoryCheckpointStore {
    type Error = MemoryError;

    async fn load(
        &self,
        vehicle: VehicleId,
    ) -> Result<(Option<StoredCheckpoint>, Option<PreparedCommit>), Self::Error> {
        let mut inner = self.lock();
        if inner.faults.remove(&Op::Load) {
            return Err(MemoryError::Injected);
        }
        let checkpoint = inner.checkpoints.get(&vehicle).cloned();
        let prepared = inner.prepared.get(&vehicle).cloned();
        Ok((checkpoint, prepared))
    }

    async fn prepare(
        &self,
        vehicle: VehicleId,
        partition: u16,
        prepared: PreparedCommit,
    ) -> Result<PrepareOutcome, Self::Error> {
        let mut inner = self.lock();
        if inner.faults.remove(&Op::Prepare) {
            return Err(MemoryError::Injected);
        }

        if let Some(existing) = inner.prepared.get(&vehicle) {
            return Ok(if existing.output == prepared.output {
                PrepareOutcome::AlreadyPrepared
            } else {
                PrepareOutcome::Busy {
                    pending: existing.output,
                }
            });
        }

        // `None` expected_base demands that no checkpoint exists at all.
        let actual = inner.checkpoints.get(&vehicle).map(|c| c.revision);
        let matches_base = match prepared.expected_base {
            None => actual.is_none(),
            Some(base) => actual == Some(base),
        };
        if !matches_base {
            return Ok(PrepareOutcome::Conflict { actual });
        }

        inner.index.entry(partition).or_default().insert(vehicle);
        inner.prepared.insert(vehicle, prepared);
        Ok(PrepareOutcome::Prepared)
    }

    async fn mark_published(
        &self,
        vehicle: VehicleId,
        output: OutputId,
    ) -> Result<(), Self::Error> {
        let mut inner = self.lock();
        if inner.faults.remove(&Op::MarkPublished) {
            return Err(MemoryError::Injected);
        }
        match inner.prepared.get_mut(&vehicle) {
            Some(prepared) if prepared.output == output => {
                prepared.phase = CommitPhase::Published;
                Ok(())
            }
            Some(prepared) => Err(MemoryError::OutputMismatch {
                staged: prepared.output,
                got: output,
            }),
            None => Ok(()),
        }
    }

    async fn promote(
        &self,
        vehicle: VehicleId,
        partition: u16,
        output: OutputId,
    ) -> Result<(), Self::Error> {
        let mut inner = self.lock();
        if inner.faults.remove(&Op::Promote) {
            return Err(MemoryError::Injected);
        }

        // Build the checkpoint before mutating, so the borrow ends before delete.
        let stored = match inner.prepared.get(&vehicle) {
            Some(prepared) if prepared.output == output => StoredCheckpoint {
                revision: prepared.next_revision,
                segment: prepared.next_segment,
                bytes: prepared.next_checkpoint.clone(),
            },
            Some(prepared) => {
                return Err(MemoryError::OutputMismatch {
                    staged: prepared.output,
                    got: output,
                });
            }
            None => return Ok(()),
        };

        inner.prepared.remove(&vehicle);
        if let Some(set) = inner.index.get_mut(&partition) {
            set.remove(&vehicle);
        }
        inner.checkpoints.insert(vehicle, stored);
        Ok(())
    }

    async fn list_prepared(
        &self,
        partition: u16,
    ) -> Result<Vec<(VehicleId, PreparedCommit)>, Self::Error> {
        let mut inner = self.lock();
        if inner.faults.remove(&Op::ListPrepared) {
            return Err(MemoryError::Injected);
        }
        let mut out: Vec<(VehicleId, PreparedCommit)> = inner
            .index
            .get(&partition)
            .into_iter()
            .flatten()
            .filter_map(|vehicle| inner.prepared.get(vehicle).map(|p| (*vehicle, p.clone())))
            .collect();
        // Deterministic order, independent of hash-map iteration.
        out.sort_by_key(|(vehicle, _)| vehicle.0);
        Ok(out)
    }

    async fn frontier(&self, partition: u16) -> Result<Option<PartitionFrontier>, Self::Error> {
        let mut inner = self.lock();
        if inner.faults.remove(&Op::Frontier) {
            return Err(MemoryError::Injected);
        }
        Ok(inner.frontiers.get(&partition).copied())
    }

    async fn set_frontier(&self, frontier: PartitionFrontier) -> Result<(), Self::Error> {
        let mut inner = self.lock();
        if inner.faults.remove(&Op::SetFrontier) {
            return Err(MemoryError::Injected);
        }
        inner.frontiers.insert(frontier.partition, frontier);
        Ok(())
    }

    async fn expire_idle(&self, vehicle: VehicleId) -> Result<(), Self::Error> {
        let mut inner = self.lock();
        if inner.faults.remove(&Op::Expire) {
            return Err(MemoryError::Injected);
        }
        // Never touch a prepared record: an in-flight commit must survive.
        inner.checkpoints.remove(&vehicle);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use routers_network::mock::MockEntryId;

    use crate::protocol::ids::SCHEMA_VERSION;

    use super::*;

    fn store() -> MemoryCheckpointStore {
        MemoryCheckpointStore::new()
    }

    fn obs(partition: u16, sequence: u64) -> ObservationId {
        ObservationId {
            partition,
            sequence,
        }
    }

    // Promotes one revision past the base it supersedes (fresh vehicle -> 1).
    fn prepared(output: u128, expected_base: Option<Revision>) -> PreparedCommit {
        PreparedCommit {
            output: OutputId(output),
            output_subject: "events.matched.v1.p.0".to_owned(),
            output_bytes: vec![0xaa, 0xbb],
            next_checkpoint: vec![1, 2, 3, 4],
            next_revision: Revision(expected_base.map_or(1, |r| r.0 + 1)),
            next_segment: SegmentId(7),
            expected_base,
            phase: CommitPhase::Prepared,
            raw: obs(0, 100),
        }
    }

    // The only way to seed the checkpoint map is a real prepare+promote.
    async fn seed_checkpoint(
        s: &MemoryCheckpointStore,
        vehicle: VehicleId,
        revision: Revision,
        segment: SegmentId,
    ) {
        let mut p = prepared(0x1000 + u128::from(revision.0), None);
        p.next_revision = revision;
        p.next_segment = segment;
        assert_eq!(
            s.prepare(vehicle, 0, p.clone()).await.unwrap(),
            PrepareOutcome::Prepared
        );
        s.promote(vehicle, 0, p.output).await.unwrap();
    }

    #[test]
    fn is_published_reflects_phase() {
        let mut p = prepared(0x1, None);
        assert!(!p.is_published());
        p.phase = CommitPhase::Published;
        assert!(p.is_published());
    }

    #[tokio::test]
    async fn prepare_stages_a_fresh_vehicle() {
        let s = store();
        let v = VehicleId(1);
        let p = prepared(0xa1, None);

        assert_eq!(
            s.prepare(v, 3, p.clone()).await.unwrap(),
            PrepareOutcome::Prepared
        );

        let (checkpoint, pending) = s.load(v).await.unwrap();
        assert!(checkpoint.is_none());
        assert_eq!(pending, Some(p.clone()));

        assert_eq!(s.list_prepared(3).await.unwrap(), vec![(v, p)]);
    }

    #[tokio::test]
    async fn prepare_same_output_is_idempotent_and_different_is_busy() {
        let s = store();
        let v = VehicleId(1);
        let first = prepared(0xa1, None);

        assert_eq!(
            s.prepare(v, 0, first.clone()).await.unwrap(),
            PrepareOutcome::Prepared
        );
        assert_eq!(
            s.prepare(v, 0, first).await.unwrap(),
            PrepareOutcome::AlreadyPrepared
        );
        assert_eq!(
            s.prepare(v, 0, prepared(0xb2, None)).await.unwrap(),
            PrepareOutcome::Busy {
                pending: OutputId(0xa1),
            }
        );
    }

    #[tokio::test]
    async fn prepare_conflicts_when_base_disagrees() {
        let s = store();
        let v = VehicleId(1);
        seed_checkpoint(&s, v, Revision(5), SegmentId(5)).await;

        // Expecting no checkpoint, but one exists.
        assert_eq!(
            s.prepare(v, 0, prepared(0xc3, None)).await.unwrap(),
            PrepareOutcome::Conflict {
                actual: Some(Revision(5)),
            }
        );
        // Expecting the wrong revision.
        assert_eq!(
            s.prepare(v, 0, prepared(0xc3, Some(Revision(4))))
                .await
                .unwrap(),
            PrepareOutcome::Conflict {
                actual: Some(Revision(5)),
            }
        );
        // Expecting the right revision.
        assert_eq!(
            s.prepare(v, 0, prepared(0xc3, Some(Revision(5))))
                .await
                .unwrap(),
            PrepareOutcome::Prepared
        );
    }

    #[tokio::test]
    async fn prepare_conflicts_when_expecting_a_missing_checkpoint() {
        let s = store();
        let v = VehicleId(2);
        assert_eq!(
            s.prepare(v, 0, prepared(0xd4, Some(Revision(9))))
                .await
                .unwrap(),
            PrepareOutcome::Conflict { actual: None }
        );
    }

    #[tokio::test]
    async fn promote_installs_checkpoint_and_clears_prepared() {
        let s = store();
        let v = VehicleId(1);
        let mut p = prepared(0xa1, None);
        p.next_revision = Revision(42);
        p.next_segment = SegmentId(7);
        p.next_checkpoint = vec![9, 8, 7];
        assert_eq!(
            s.prepare(v, 4, p.clone()).await.unwrap(),
            PrepareOutcome::Prepared
        );

        s.promote(v, 4, p.output).await.unwrap();

        let (checkpoint, pending) = s.load(v).await.unwrap();
        assert_eq!(pending, None);
        assert_eq!(
            checkpoint,
            Some(StoredCheckpoint {
                revision: Revision(42),
                segment: SegmentId(7),
                bytes: vec![9, 8, 7],
            })
        );
        assert_eq!(s.list_prepared(4).await.unwrap(), vec![]);

        // Promoting again is an idempotent no-op.
        s.promote(v, 4, p.output).await.unwrap();
        assert_eq!(s.load(v).await.unwrap().0.unwrap().revision, Revision(42));
    }

    #[tokio::test]
    async fn promote_rejects_a_different_output() {
        let s = store();
        let v = VehicleId(1);
        s.prepare(v, 0, prepared(0xa1, None)).await.unwrap();
        let err = s.promote(v, 0, OutputId(0xffff)).await.unwrap_err();
        assert!(matches!(err, MemoryError::OutputMismatch { .. }));
        assert!(s.load(v).await.unwrap().1.is_some());
    }

    #[tokio::test]
    async fn mark_published_sets_phase_and_rejects_other_output() {
        let s = store();
        let v = VehicleId(1);
        let p = prepared(0xa1, None);
        s.prepare(v, 0, p.clone()).await.unwrap();

        // Wrong output: error, phase unchanged.
        let err = s.mark_published(v, OutputId(0xffff)).await.unwrap_err();
        assert!(matches!(err, MemoryError::OutputMismatch { .. }));
        assert!(!s.load(v).await.unwrap().1.unwrap().is_published());

        // Right output, then idempotent on a second call.
        s.mark_published(v, p.output).await.unwrap();
        assert!(s.load(v).await.unwrap().1.unwrap().is_published());
        s.mark_published(v, p.output).await.unwrap();
        assert!(s.load(v).await.unwrap().1.unwrap().is_published());
    }

    #[tokio::test]
    async fn list_prepared_is_scoped_to_a_partition() {
        let s = store();
        let (a, b, c) = (VehicleId(1), VehicleId(2), VehicleId(3));
        s.prepare(a, 0, prepared(0xa, None)).await.unwrap();
        s.prepare(b, 0, prepared(0xb, None)).await.unwrap();
        s.prepare(c, 1, prepared(0xc, None)).await.unwrap();

        let p0: Vec<VehicleId> = s
            .list_prepared(0)
            .await
            .unwrap()
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        assert_eq!(p0, vec![a, b]);

        let p1: Vec<VehicleId> = s
            .list_prepared(1)
            .await
            .unwrap()
            .into_iter()
            .map(|(v, _)| v)
            .collect();
        assert_eq!(p1, vec![c]);

        assert_eq!(s.list_prepared(99).await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn frontier_round_trips_per_partition() {
        let s = store();
        assert_eq!(s.frontier(7).await.unwrap(), None);

        s.set_frontier(PartitionFrontier {
            partition: 7,
            sequence: 1234,
        })
        .await
        .unwrap();
        assert_eq!(
            s.frontier(7).await.unwrap(),
            Some(PartitionFrontier {
                partition: 7,
                sequence: 1234,
            })
        );

        // Overwrites in place.
        s.set_frontier(PartitionFrontier {
            partition: 7,
            sequence: 2000,
        })
        .await
        .unwrap();
        assert_eq!(s.frontier(7).await.unwrap().unwrap().sequence, 2000);

        assert_eq!(s.frontier(8).await.unwrap(), None);
    }

    #[tokio::test]
    async fn expire_idle_drops_checkpoint_but_keeps_prepared() {
        let s = store();
        let v = VehicleId(1);
        seed_checkpoint(&s, v, Revision(5), SegmentId(5)).await;
        let p = prepared(0xe5, Some(Revision(5)));
        s.prepare(v, 0, p.clone()).await.unwrap();

        s.expire_idle(v).await.unwrap();

        let (checkpoint, pending) = s.load(v).await.unwrap();
        assert_eq!(checkpoint, None, "the committed checkpoint is dropped");
        assert_eq!(pending, Some(p), "the prepared record is untouched");
    }

    // Flatten each op's result to `Result<(), MemoryError>` for the fault table.
    async fn invoke(s: &MemoryCheckpointStore, op: Op) -> Result<(), MemoryError> {
        let v = VehicleId(7);
        match op {
            Op::Load => s.load(v).await.map(|_| ()),
            Op::Prepare => s.prepare(v, 0, prepared(0x1, None)).await.map(|_| ()),
            Op::MarkPublished => s.mark_published(v, OutputId(0x1)).await,
            Op::Promote => s.promote(v, 0, OutputId(0x1)).await,
            Op::ListPrepared => s.list_prepared(0).await.map(|_| ()),
            Op::Frontier => s.frontier(0).await.map(|_| ()),
            Op::SetFrontier => {
                s.set_frontier(PartitionFrontier {
                    partition: 0,
                    sequence: 0,
                })
                .await
            }
            Op::Expire => s.expire_idle(v).await,
        }
    }

    #[tokio::test]
    async fn every_op_honours_its_injected_fault_once() {
        let ops = [
            Op::Load,
            Op::Prepare,
            Op::MarkPublished,
            Op::Promote,
            Op::ListPrepared,
            Op::Frontier,
            Op::SetFrontier,
            Op::Expire,
        ];
        for op in ops {
            let s = store();
            s.fail_next(op);
            assert!(
                matches!(invoke(&s, op).await, Err(MemoryError::Injected)),
                "{op:?} should fail once"
            );
            assert!(
                invoke(&s, op).await.is_ok(),
                "{op:?} should succeed after the fault clears"
            );
        }
    }

    #[tokio::test]
    async fn snapshot_is_a_deep_copy_and_state_loss_helpers_work() {
        let s = store();
        let v = VehicleId(1);
        seed_checkpoint(&s, v, Revision(5), SegmentId(5)).await;
        s.prepare(v, 2, prepared(0xf6, Some(Revision(5))))
            .await
            .unwrap();
        s.set_frontier(PartitionFrontier {
            partition: 2,
            sequence: 10,
        })
        .await
        .unwrap();

        let snap = s.snapshot();
        assert_eq!(snap.checkpoints.len(), 1);
        assert_eq!(snap.prepared.len(), 1);
        assert_eq!(snap.frontiers.get(&2).unwrap().sequence, 10);
        assert!(snap.index.get(&2).unwrap().contains(&v));

        // Dropping checkpoints leaves the prepared record.
        s.drop_all_checkpoints();
        let (checkpoint, pending) = s.load(v).await.unwrap();
        assert!(checkpoint.is_none());
        assert!(pending.is_some());

        // Dropping the prepared record clears it and its index entry.
        s.drop_prepared(v);
        assert!(s.load(v).await.unwrap().1.is_none());
        assert!(s.list_prepared(2).await.unwrap().is_empty());

        // The earlier snapshot was a deep copy and is unaffected.
        assert_eq!(snap.prepared.len(), 1);
        assert_eq!(snap.checkpoints.len(), 1);
    }

    #[test]
    fn vehicle_checkpoint_round_trips() {
        let checkpoint = VehicleCheckpoint::<MockEntryId> {
            trip: Trip::new(),
            last_input: obs(485, 42),
            revision: Revision(42),
            segment: SegmentId(7),
            finalized_through: Some(1_700_000_000_000_000),
            graph: GraphVersion::new("g1").unwrap(),
            schema: SCHEMA_VERSION,
            region: RegionId::new("r1").unwrap(),
        };

        let bytes = checkpoint.encode().expect("encode");
        let decoded = VehicleCheckpoint::<MockEntryId>::decode(&bytes).expect("decode");

        // `Trip` lacks `PartialEq`; re-encode to prove the structure survived.
        assert_eq!(decoded.encode().expect("re-encode"), bytes);
        assert_eq!(decoded.last_input, checkpoint.last_input);
        assert_eq!(decoded.revision, Revision(42));
        assert_eq!(decoded.segment, SegmentId(7));
        assert_eq!(decoded.finalized_through, Some(1_700_000_000_000_000));
        assert_eq!(decoded.graph, checkpoint.graph);
        assert_eq!(decoded.schema, SCHEMA_VERSION);
        assert_eq!(decoded.region, checkpoint.region);
    }

    #[test]
    fn to_stored_lifts_revision_and_segment_out_of_the_bytes() {
        let checkpoint = VehicleCheckpoint::<MockEntryId> {
            trip: Trip::new(),
            last_input: obs(0, 1),
            revision: Revision(9),
            segment: SegmentId(3),
            finalized_through: None,
            graph: GraphVersion::new("g").unwrap(),
            schema: SCHEMA_VERSION,
            region: RegionId::new("r").unwrap(),
        };

        let stored = checkpoint.to_stored().expect("to_stored");
        assert_eq!(stored.revision, Revision(9));
        assert_eq!(stored.segment, SegmentId(3));
        assert_eq!(stored.bytes, checkpoint.encode().unwrap());
    }
}
