//! Checkpoint records and the [`CheckpointStore`] trait, plus an in-memory
//! implementation for tests. (T10)
//!
//! A vehicle's durable state is two things, kept deliberately apart:
//!
//! * the *committed* [`VehicleCheckpoint`] — the resume state the last decided
//!   revision left behind; and
//! * an optional *prepared* [`PreparedCommit`] — an output that has been staged
//!   (and perhaps already published) but whose checkpoint has not yet been
//!   promoted.
//!
//! Keeping them apart is what makes a commit crash-safe. The orchestrator
//! writes the prepared record first ([`prepare`](CheckpointStore::prepare)),
//! publishes the output to the broker, then promotes
//! ([`promote`](CheckpointStore::promote)) — installing the next checkpoint and
//! dropping the prepared record in one atomic step. A crash at any point leaves
//! exactly one recoverable state, and every step is idempotent, so recovery
//! simply re-drives whatever prepared record it finds.
//!
//! The store never decodes a checkpoint: the encoded [`VehicleCheckpoint`]
//! bytes are opaque to it, because it cannot name the network [`Entry`] type
//! `E`. Yet [`prepare`](CheckpointStore::prepare) must compare the stored
//! *revision* against the caller's expected base to detect a lost race. So the
//! revision (and its segment) travel *outside* the opaque bytes, in
//! [`StoredCheckpoint`]. This is the one refinement to DESIGN §7 — which
//! returned a bare `Vec<u8>` from `load` — and it is recorded in the task's
//! deviations.

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

/// The committed resume state for one vehicle: everything a matcher needs to
/// carry the vehicle's match on from where the last decision left off, plus the
/// provenance the orchestrator reasons about staleness with.
///
/// This is what [`promote`](CheckpointStore::promote) installs and
/// [`load`](CheckpointStore::load) returns (as the opaque bytes inside a
/// [`StoredCheckpoint`]). It is generic over the network [`Entry`] type because
/// it carries a live [`Trip`]; the store, which cannot name `E`, only ever sees
/// the [`encode`](Self::encode)d bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct VehicleCheckpoint<E: Entry> {
    /// The retained trip (origins, candidates, trellis) the next solve resumes.
    pub trip: Trip<E>,
    /// The last observation folded into this checkpoint.
    pub last_input: ObservationId,
    /// The revision this checkpoint was committed at. The next commit must
    /// resume from exactly this revision or it has lost a race.
    pub revision: Revision,
    /// The continuity segment this checkpoint belongs to.
    pub segment: SegmentId,
    /// The finality watermark: layers at or below this timestamp are final and
    /// must never be rewritten by a later revision.
    pub finalized_through: Option<i64>,
    /// The graph snapshot the retained trip was solved against.
    pub graph: GraphVersion,
    /// The wire schema this record was written under.
    pub schema: SchemaVersion,
    /// The region that owns the vehicle's solves.
    pub region: RegionId,
}

impl<E: Entry> VehicleCheckpoint<E> {
    /// Serialise to the opaque bytes the store persists. Postcard, so the
    /// encoding is compact and stable for a fixed field order.
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
    /// needs out-of-band — it cannot decode `E` to read them. This is the
    /// record a commit hands to [`promote`](CheckpointStore::promote) (via
    /// [`PreparedCommit::next_checkpoint`]) and that the store returns from
    /// [`load`](CheckpointStore::load).
    pub fn to_stored(&self) -> Result<StoredCheckpoint, postcard::Error> {
        Ok(StoredCheckpoint {
            revision: self.revision,
            segment: self.segment,
            bytes: self.encode()?,
        })
    }
}

/// A committed checkpoint as the store holds it: the opaque
/// [`VehicleCheckpoint`] bytes, with the `revision` and `segment` lifted out so
/// the store can compare bases and answer [`load`](CheckpointStore::load)
/// without ever decoding the [`Entry`] type sealed inside the bytes.
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
/// checkpoint to install once that publish is durable.
///
/// Written by [`prepare`](CheckpointStore::prepare) before the output is
/// published and consumed by [`promote`](CheckpointStore::promote) after.
/// Retained with *no* expiry so a crash mid-commit is always recoverable: the
/// prepared record is the durable record of intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedCommit {
    /// The committed output's identity, and its broker dedup key. Two calls
    /// carrying the same `output` are the same logical commit.
    pub output: OutputId,
    /// The subject the output bytes publish on.
    pub output_subject: String,
    /// The exact bytes to publish — byte-for-byte identical on every retry, so
    /// the broker dedups a re-publish rather than emitting a second output.
    pub output_bytes: Vec<u8>,
    /// The encoded next [`VehicleCheckpoint`]; opaque to the store and installed
    /// verbatim by [`promote`](CheckpointStore::promote).
    pub next_checkpoint: Vec<u8>,
    /// The revision the promoted checkpoint will carry, lifted out of the
    /// opaque bytes so `promote` can build a [`StoredCheckpoint`] without
    /// decoding `E`.
    pub next_revision: Revision,
    /// The segment the promoted checkpoint will carry (see `next_revision`).
    pub next_segment: SegmentId,
    /// The revision this commit expected to supersede — the compare guard
    /// [`prepare`](CheckpointStore::prepare) checks against the stored
    /// checkpoint. `None` means "no checkpoint may exist yet" (a fresh vehicle).
    pub expected_base: Option<Revision>,
    /// How far this commit has progressed (see [`CommitPhase`]).
    pub phase: CommitPhase,
    /// The raw observation that triggered this commit, kept for completion
    /// tracking once the commit is promoted.
    pub raw: ObservationId,
}

impl PreparedCommit {
    /// Whether the output is already durable on the broker, so recovery need
    /// only promote this record rather than republish its bytes.
    pub fn is_published(&self) -> bool {
        matches!(self.phase, CommitPhase::Published)
    }
}

/// The high-water mark of a partition's raw journal: the last raw sequence
/// whose commit is fully durable. Recovery resumes consuming that partition's
/// stream from `sequence + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PartitionFrontier {
    /// The partition this frontier belongs to.
    pub partition: u16,
    /// The last durably-committed raw sequence in that partition.
    pub sequence: u64,
}

/// The result of a [`prepare`](CheckpointStore::prepare) attempt — the four ways
/// a compare-and-stage can land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrepareOutcome {
    /// The record was staged: the compare matched and no prepared record
    /// existed.
    Prepared,
    /// A prepared record with the *same* output already exists; the call was a
    /// no-op. [`prepare`](CheckpointStore::prepare) is therefore safe to retry.
    AlreadyPrepared,
    /// The stored checkpoint's revision did not match `expected_base`; another
    /// commit won the race. `actual` is the revision actually stored, or `None`
    /// if no checkpoint exists.
    Conflict {
        /// The revision the store actually holds for the vehicle.
        actual: Option<Revision>,
    },
    /// A prepared record for a *different* output is already staged; the vehicle
    /// is mid-commit and must be drained before a new commit can begin.
    Busy {
        /// The output of the commit already in flight.
        pending: OutputId,
    },
}

/// The durable home of every vehicle's committed checkpoint and in-flight
/// prepared commit, plus each partition's raw-journal frontier.
///
/// The trait carries no business logic: it persists and hands back opaque bytes
/// and small records, and guarantees the atomicity each method documents. The
/// Valkey implementation (T11) realises those guarantees with per-vehicle Lua
/// scripts over a single key group; [`MemoryCheckpointStore`] realises them with
/// one mutex. Every method is `async` so a real store can do I/O; the in-memory
/// fake completes synchronously.
#[allow(async_fn_in_trait)]
pub trait CheckpointStore: Clone + Send + Sync + 'static {
    /// The failure a store operation can report. Concrete stores choose their
    /// own; callers only need it to be a real, shareable error.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Load a vehicle's durable state: its committed checkpoint (if any) and its
    /// in-flight prepared commit (if any). A single point-in-time read, so the
    /// two halves are mutually consistent.
    async fn load(
        &self,
        vehicle: VehicleId,
    ) -> Result<(Option<StoredCheckpoint>, Option<PreparedCommit>), Self::Error>;

    /// Compare-and-stage a commit for `vehicle` in `partition`. Atomically, in
    /// one critical section over this vehicle's keys:
    ///
    /// 1. if a prepared record already exists, return
    ///    [`PrepareOutcome::AlreadyPrepared`] when its `output` equals
    ///    `prepared.output`, else [`PrepareOutcome::Busy`] naming the pending
    ///    output — never overwrite a different in-flight commit;
    /// 2. otherwise compare the stored checkpoint's revision against
    ///    `prepared.expected_base` (`None` demands that no checkpoint exists);
    ///    on mismatch return [`PrepareOutcome::Conflict`] carrying the actual
    ///    stored revision and stage nothing;
    /// 3. otherwise write the prepared record and add `vehicle` to `partition`'s
    ///    prepared index, returning [`PrepareOutcome::Prepared`].
    ///
    /// The revision compared in step 2 is read from [`StoredCheckpoint`], never
    /// by decoding the opaque bytes.
    async fn prepare(
        &self,
        vehicle: VehicleId,
        partition: u16,
        prepared: PreparedCommit,
    ) -> Result<PrepareOutcome, Self::Error>;

    /// Mark the vehicle's prepared commit as [`CommitPhase::Published`],
    /// recording that its output is durable on the broker. Atomic over the
    /// prepared record and idempotent: a second call with the same `output` is a
    /// no-op. It must error if a prepared record exists with a *different*
    /// output (a stale caller); with no prepared record — already promoted away
    /// — there is nothing to mark and the call succeeds.
    async fn mark_published(&self, vehicle: VehicleId, output: OutputId)
    -> Result<(), Self::Error>;

    /// Promote the vehicle's prepared commit in `partition`. Atomically: install
    /// the [`StoredCheckpoint`] built from the prepared record's
    /// `next_checkpoint` bytes and its `next_revision`/`next_segment`, delete the
    /// prepared record, and remove `vehicle` from the partition's prepared
    /// index. Idempotent when already promoted (no prepared record): the
    /// checkpoint stays as written and the call succeeds. It must error if a
    /// prepared record for a *different* output is staged.
    async fn promote(
        &self,
        vehicle: VehicleId,
        partition: u16,
        output: OutputId,
    ) -> Result<(), Self::Error>;

    /// Every staged prepared commit in `partition`, so recovery can re-drive
    /// them. The partition index is best-effort — the store repairs it — so a
    /// caller must tolerate a listed vehicle that has since been promoted away.
    async fn list_prepared(
        &self,
        partition: u16,
    ) -> Result<Vec<(VehicleId, PreparedCommit)>, Self::Error>;

    /// The partition's raw-journal frontier, or `None` if none has been set.
    async fn frontier(&self, partition: u16) -> Result<Option<PartitionFrontier>, Self::Error>;

    /// Set (overwrite) a partition's frontier. The caller advances it
    /// monotonically; the store just records what it is given.
    async fn set_frontier(&self, frontier: PartitionFrontier) -> Result<(), Self::Error>;

    /// Drop a vehicle's *committed* checkpoint because it has gone idle. It must
    /// never touch a prepared record: an idle vehicle mid-commit keeps its
    /// staged state so the commit can still complete.
    async fn expire_idle(&self, vehicle: VehicleId) -> Result<(), Self::Error>;
}

/// Which [`MemoryCheckpointStore`] operation a fault has been armed against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    /// [`CheckpointStore::load`].
    Load,
    /// [`CheckpointStore::prepare`].
    Prepare,
    /// [`CheckpointStore::mark_published`].
    MarkPublished,
    /// [`CheckpointStore::promote`].
    Promote,
    /// [`CheckpointStore::list_prepared`].
    ListPrepared,
    /// [`CheckpointStore::frontier`].
    Frontier,
    /// [`CheckpointStore::set_frontier`].
    SetFrontier,
    /// [`CheckpointStore::expire_idle`].
    Expire,
}

/// The error a [`MemoryCheckpointStore`] can report.
#[derive(Debug, Error)]
pub enum MemoryError {
    /// A fault armed by [`MemoryCheckpointStore::fail_next`] fired.
    #[error("injected memory-store fault")]
    Injected,
    /// An operation named an output that disagreed with the staged prepared
    /// record — a stale or crossed caller.
    #[error("prepared output mismatch: staged {staged}, got {got}")]
    OutputMismatch {
        /// The output the store has staged for the vehicle.
        staged: OutputId,
        /// The output the caller named.
        got: OutputId,
    },
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
/// the Valkey key-group atomicity, with knobs to inject faults and to simulate
/// state loss. Cloneable and shareable — every clone shares one backing store.
#[derive(Clone, Default)]
pub struct MemoryCheckpointStore {
    inner: Arc<Mutex<Inner>>,
}

/// A deep copy of a [`MemoryCheckpointStore`]'s state, taken for test
/// assertions; later mutations of the store do not touch it.
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

    /// Arm a one-shot fault: the *next* call of `op` returns
    /// [`MemoryError::Injected`], then disarms. Several distinct ops may be
    /// armed at once; each fires exactly once.
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
    /// simulate the "checkpoint missing but prepared exists" state loss the
    /// recovery path must survive.
    pub fn drop_all_checkpoints(&self) {
        self.lock().checkpoints.clear();
    }

    /// Drop one vehicle's prepared record (and its index entry) to simulate a
    /// lost prepared commit.
    pub fn drop_prepared(&self, vehicle: VehicleId) {
        let mut inner = self.lock();
        inner.prepared.remove(&vehicle);
        for set in inner.index.values_mut() {
            set.remove(&vehicle);
        }
    }

    /// Lock the inner state, translating a poisoned mutex into a panic — a
    /// poisoned lock only happens if another operation panicked mid-mutation,
    /// which never occurs on these infallible in-memory paths.
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

        // A prepared record already in flight decides the outcome on its own:
        // the same output is a safe retry, a different one means the vehicle is
        // busy. Neither compares against the checkpoint.
        if let Some(existing) = inner.prepared.get(&vehicle) {
            return Ok(if existing.output == prepared.output {
                PrepareOutcome::AlreadyPrepared
            } else {
                PrepareOutcome::Busy {
                    pending: existing.output,
                }
            });
        }

        // No prepared record: compare the stored revision against the expected
        // base. `None` demands that no checkpoint exists at all.
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
            // Already promoted away: nothing to mark.
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

        // Read out what the checkpoint becomes before mutating anything, so the
        // borrow of the prepared record ends before we delete it.
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
            // Already promoted: the checkpoint stays as written.
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
        // Deterministic order, so callers and assertions do not depend on the
        // hash-map iteration order.
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
        // Only the committed checkpoint is idle-expired; a prepared record is
        // the durable record of an in-flight commit and is never touched here.
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

    /// A prepared commit whose promoted checkpoint lands one revision past the
    /// base it supersedes (a fresh vehicle promotes to revision 1).
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

    /// Drive a vehicle to a committed checkpoint at `revision`/`segment` in
    /// partition 0 (the only way to seed the checkpoint map is a real commit).
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

        // `load` reflects the staged record and no checkpoint yet.
        let (checkpoint, pending) = s.load(v).await.unwrap();
        assert!(checkpoint.is_none());
        assert_eq!(pending, Some(p.clone()));

        // And it is discoverable through the partition index.
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
        // Same output: a safe retry.
        assert_eq!(
            s.prepare(v, 0, first).await.unwrap(),
            PrepareOutcome::AlreadyPrepared
        );
        // A different output while one is in flight: busy, naming the pending
        // output and staging nothing new.
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
        // Expecting the right revision: staged.
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

        // The checkpoint carries the prepared record's next revision/segment
        // and bytes, and the prepared record is gone.
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
        // The partition index no longer lists the vehicle.
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
        // The staged record survives a rejected promote.
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

        // Right output: published, and idempotent on a second call.
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

        // A different partition is unaffected.
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

    /// Invoke one operation once, flattening its distinct result types to
    /// `Result<(), MemoryError>` so the fault-injection table can drive them all.
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

        // Dropping checkpoints leaves the prepared record (state-loss sim).
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

        // `Trip` lacks `PartialEq`, so re-encode to prove the whole structure
        // survived (the same trick output.rs uses).
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
