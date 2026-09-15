//! Completion-frontier tracker (spec §3).
//!
//! A partition worker consumes its raw journal through a *filtered* view of a
//! shared JetStream stream: the stream carries every partition's raw messages
//! interleaved, and this consumer is told to deliver only the subject for its
//! own partition. Delivered stream sequences therefore arrive **in order but
//! with gaps** — the sequences belonging to other partitions are simply skipped.
//!
//! Recovery re-opens delivery with `DeliverPolicy::ByStartSequence` from
//! `frontier + 1`, so the worker must persist a single number per partition: the
//! greatest stream sequence below which nothing of *ours* is still in flight.
//! That number is the completion frontier `F`.
//!
//! # What the frontier means
//!
//! Two events bracket a raw message's life in the worker:
//!
//! * [`observe`](FrontierTracker::observe) — the message has been *delivered*,
//!   before any processing. From this point the worker is responsible for it.
//! * [`complete`](FrontierTracker::complete) — its commit has been *promoted*
//!   (or it was suppressed as already-committed, or poison-acked). The worker is
//!   done with it and a restart need never see it again.
//!
//! `F` is the greatest position such that no observed-but-incomplete sequence is
//! at or below it:
//!
//! ```text
//! F = if outstanding.is_empty() { max_completed } else { min(outstanding) - 1 }
//! ```
//!
//! and `F` never decreases. Because the smallest still-outstanding sequence is
//! `min(outstanding)`, everything up to `min(outstanding) - 1` is safe to skip
//! on restart — including the gap sequences that were never delivered here.
//! Restarting at `F + 1` re-reads exactly the outstanding messages plus anything
//! never delivered, which is the smallest correct replay. The frontier *may lag*
//! commits (persistence is batched) but must *never lead* them (§2), which is why
//! it only ever advances on completion and never past an outstanding sequence.
//!
//! # Why gaps are not holes
//!
//! Observing sequences `10, 20, 30` with nothing in between does not stall the
//! frontier at `9`: the missing `11..=19` were never our messages, so once `10`
//! completes the frontier jumps straight to `19`. A *hole* — a sequence we
//! observed but have not completed — is the only thing that holds the frontier
//! back, and it holds it at exactly the sequence *before* the oldest such hole.

use alloc::collections::BTreeSet;
use core::time::Duration;

use tokio::time::Instant;
use tracing::warn;

use crate::store::checkpoint::PartitionFrontier;

/// How often the tracker asks to be persisted.
///
/// Persistence is a batched side effect: the frontier is advisory (it only ever
/// shrinks the replay set on restart, never affects correctness of the match),
/// so writing it on every completion would be pure overhead. The tracker asks
/// for a write when *either* enough completions have accumulated *or* enough
/// wall-clock time has passed since the last write — see
/// [`due`](FrontierTracker::due).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrontierConfig {
    /// Persist once this many completions have landed since the last write. The
    /// count trigger bounds how far the persisted frontier can lag under load.
    pub every: u32,
    /// Persist at least this often even when completions are sparse, so a quiet
    /// partition still checkpoints its progress and shortens its own replay.
    pub at_least_every: Duration,
}

impl Default for FrontierConfig {
    /// The spec defaults: every 64 completions, and at least once per second.
    fn default() -> Self {
        Self {
            every: 64,
            at_least_every: Duration::from_secs(1),
        }
    }
}

/// The outcome of [`observe`](FrontierTracker::observe): what the delivered
/// sequence was relative to state the tracker already holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observed {
    /// A sequence not seen before; it is now outstanding.
    New,
    /// A sequence already outstanding — a redelivery of something still in
    /// flight. The caller should not double-process it.
    Duplicate,
    /// A sequence at or below the frontier — a redelivery of something already
    /// terminal. The caller should ack it and move on without processing.
    BehindFrontier,
}

/// How far [`complete`](FrontierTracker::complete) moved the frontier. `from`
/// and `to` are the frontier immediately before and after the completion;
/// `from == to` when the completed sequence was not the oldest hole and so
/// nothing moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advance {
    /// The frontier before this completion.
    pub from: u64,
    /// The frontier after this completion.
    pub to: u64,
}

impl Advance {
    /// Whether the frontier actually moved.
    pub fn advanced(&self) -> bool {
        self.to > self.from
    }
}

/// Tracks out-of-order completions for one partition and reports the contiguous
/// completed prefix of its filtered raw input as the frontier `F`.
///
/// The tracker holds no vehicle or job state: it reasons purely about raw stream
/// sequences and their two lifecycle events. One lives inside each partition
/// worker; there is no cross-partition sharing.
#[derive(Clone, Debug)]
pub struct FrontierTracker {
    /// The partition this tracker belongs to; stamped onto every
    /// [`PartitionFrontier`] it emits.
    partition: u16,
    /// The current frontier `F`. Monotonic non-decreasing.
    frontier: u64,
    /// The greatest sequence ever completed. When nothing is outstanding the
    /// frontier equals this.
    max_completed: u64,
    /// Observed-but-not-completed sequences, ordered so the oldest hole is
    /// `first()`. All entries are strictly greater than `frontier`.
    outstanding: BTreeSet<u64>,
    /// The frontier value most recently confirmed persisted by the caller.
    persisted: u64,
    /// Completions accumulated since the last persist, for the count trigger.
    completions_since_persist: u32,
    /// When the last persist was confirmed, for the time trigger.
    last_persist: Instant,
    /// The persistence cadence.
    cfg: FrontierConfig,
}

impl FrontierTracker {
    /// Build a tracker for `partition`. `initial` is the frontier recovered from
    /// the store (`None` for a partition that has never checkpointed): the
    /// frontier, the persisted mark, and `max_completed` all start there, so
    /// any redelivery at or below it is reported [`Observed::BehindFrontier`].
    pub fn new(partition: u16, initial: Option<PartitionFrontier>, now: Instant) -> Self {
        Self::with_config(partition, initial, now, FrontierConfig::default())
    }

    /// [`new`](Self::new) with an explicit [`FrontierConfig`] (the persistence
    /// cadence). Splitting this out keeps the common constructor terse while
    /// letting callers — and tests — tune the batching triggers.
    pub fn with_config(
        partition: u16,
        initial: Option<PartitionFrontier>,
        now: Instant,
        cfg: FrontierConfig,
    ) -> Self {
        let start = initial.map_or(0, |f| {
            debug_assert_eq!(
                f.partition, partition,
                "recovered frontier for the wrong partition",
            );
            f.sequence
        });
        Self {
            partition,
            frontier: start,
            max_completed: start,
            outstanding: BTreeSet::new(),
            persisted: start,
            completions_since_persist: 0,
            last_persist: now,
            cfg,
        }
    }

    /// Record that `seq` has been delivered, before any processing.
    ///
    /// A sequence at or below the frontier is already terminal, so it is a
    /// redelivery and is reported [`Observed::BehindFrontier`] without being
    /// tracked. A sequence already outstanding is a [`Observed::Duplicate`].
    /// Otherwise it becomes outstanding and is reported [`Observed::New`].
    ///
    /// Observing may *raise* the frontier — when it establishes that the gap
    /// below the new oldest sequence holds nothing of ours — but it never
    /// lowers it, so no separate advance is reported here.
    pub fn observe(&mut self, seq: u64) -> Observed {
        if seq <= self.frontier {
            return Observed::BehindFrontier;
        }
        if !self.outstanding.insert(seq) {
            return Observed::Duplicate;
        }
        self.recompute();
        Observed::New
    }

    /// Record that `seq`'s commit is promoted (or that it was otherwise made
    /// terminal) and advance the frontier over any prefix this unblocks.
    ///
    /// Completing a sequence that was never observed is a programming error: it
    /// trips a `debug_assert!`, is logged, and is then treated as an
    /// observe-then-complete so a release build still makes progress. Completing
    /// a sequence already behind the frontier is a harmless no-op.
    pub fn complete(&mut self, seq: u64) -> Advance {
        let from = self.frontier;
        if seq <= self.frontier {
            // Already terminal — a redelivery being re-acked. Nothing moves.
            return Advance { from, to: from };
        }
        if !self.outstanding.remove(&seq) {
            debug_assert!(
                false,
                "completing unobserved sequence {seq} in partition {}",
                self.partition,
            );
            warn!(
                partition = self.partition,
                sequence = seq,
                "completing a sequence that was never observed",
            );
        }
        self.max_completed = self.max_completed.max(seq);
        self.completions_since_persist = self.completions_since_persist.saturating_add(1);
        let to = self.recompute();
        Advance { from, to }
    }

    /// Recompute the frontier from `outstanding` and `max_completed`, clamped so
    /// it never decreases, and return the new value.
    fn recompute(&mut self) -> u64 {
        let candidate = match self.outstanding.iter().next() {
            Some(&oldest) => oldest - 1,
            None => self.max_completed,
        };
        self.frontier = self.frontier.max(candidate);
        self.frontier
    }

    /// The current frontier `F`.
    pub fn frontier(&self) -> u64 {
        self.frontier
    }

    /// How many sequences are observed but not yet completed.
    pub fn outstanding(&self) -> usize {
        self.outstanding.len()
    }

    /// The oldest still-incomplete sequence — the hole holding the frontier
    /// back — or `None` when everything observed has completed.
    pub fn oldest_outstanding(&self) -> Option<u64> {
        self.outstanding.iter().next().copied()
    }

    /// Whether the frontier is worth persisting now, and if so the record to
    /// write.
    ///
    /// A write is due when the frontier has advanced past what is on disk
    /// (`frontier > persisted`) *and* either trigger has fired: enough
    /// completions since the last write, or enough time since it. The caller
    /// (the worker) performs the store write and then calls
    /// [`persisted`](Self::persisted) to confirm it.
    pub fn due(&self, now: Instant) -> Option<PartitionFrontier> {
        if self.frontier <= self.persisted {
            return None;
        }
        let by_count = self.completions_since_persist >= self.cfg.every;
        let by_time = now.saturating_duration_since(self.last_persist) >= self.cfg.at_least_every;
        (by_count || by_time).then_some(PartitionFrontier {
            partition: self.partition,
            sequence: self.frontier,
        })
    }

    /// Confirm that the caller durably persisted the frontier up to `seq`,
    /// resetting both batching triggers. `seq` is normally the `sequence` from a
    /// [`due`](Self::due) record; the persisted mark only ever advances.
    pub fn persisted(&mut self, seq: u64, now: Instant) {
        self.persisted = self.persisted.max(seq);
        self.completions_since_persist = 0;
        self.last_persist = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tracker for partition 0 starting from a clean slate at `t0`.
    fn fresh(now: Instant) -> FrontierTracker {
        FrontierTracker::new(0, None, now)
    }

    #[test]
    fn fresh_partition_starts_at_zero() {
        let t = fresh(Instant::now());
        assert_eq!(t.frontier(), 0);
        assert_eq!(t.outstanding(), 0);
        assert_eq!(t.oldest_outstanding(), None);
    }

    #[test]
    fn in_order_completions_advance_one_by_one() {
        let t0 = Instant::now();
        let mut t = fresh(t0);
        for seq in 1..=3 {
            assert_eq!(t.observe(seq), Observed::New);
        }
        // Frontier sits just below the oldest observed sequence.
        assert_eq!(t.frontier(), 0);

        assert_eq!(t.complete(1), Advance { from: 0, to: 1 });
        assert_eq!(t.complete(2), Advance { from: 1, to: 2 });
        assert_eq!(t.complete(3), Advance { from: 2, to: 3 });
        assert_eq!(t.outstanding(), 0);
    }

    #[test]
    fn out_of_order_holds_then_jumps() {
        let mut t = fresh(Instant::now());
        for seq in 3..=5 {
            t.observe(seq);
        }
        // Observing 3,4,5 skips the never-delivered 1,2: frontier is 2.
        assert_eq!(t.frontier(), 2);

        // Completing 5 before 3 moves nothing: 3 is still the oldest hole.
        let held = t.complete(5);
        assert_eq!(held, Advance { from: 2, to: 2 });
        assert!(!held.advanced());
        assert_eq!(t.oldest_outstanding(), Some(3));

        // Filling in from the oldest hole releases the whole prefix.
        assert_eq!(t.complete(3), Advance { from: 2, to: 3 });
        assert_eq!(t.complete(4), Advance { from: 3, to: 5 });
        assert_eq!(t.frontier(), 5);
        assert_eq!(t.outstanding(), 0);
    }

    #[test]
    fn gaps_in_delivered_sequences_are_not_holes() {
        let mut t = fresh(Instant::now());
        t.observe(10);
        t.observe(20);
        t.observe(30);
        // 11..=19 and 21..=29 were never ours, so the frontier sits at 9.
        assert_eq!(t.frontier(), 9);

        assert_eq!(t.complete(10), Advance { from: 9, to: 19 });
        assert_eq!(t.complete(20), Advance { from: 19, to: 29 });
        assert_eq!(t.complete(30), Advance { from: 29, to: 30 });
    }

    #[test]
    fn frontier_never_regresses() {
        let mut t = fresh(Instant::now());
        t.observe(5);
        t.complete(5);
        assert_eq!(t.frontier(), 5);

        // Anything at or below the frontier is a redelivery, never re-tracked,
        // and cannot pull the frontier back.
        assert_eq!(t.observe(5), Observed::BehindFrontier);
        assert_eq!(t.observe(3), Observed::BehindFrontier);
        assert_eq!(t.complete(4), Advance { from: 5, to: 5 });
        assert_eq!(t.frontier(), 5);
        assert_eq!(t.outstanding(), 0);
    }

    #[test]
    fn redelivery_classification() {
        let mut t = fresh(Instant::now());
        assert_eq!(t.observe(7), Observed::New);
        // Same sequence, still in flight.
        assert_eq!(t.observe(7), Observed::Duplicate);
        t.complete(7);
        // Same sequence, now terminal.
        assert_eq!(t.observe(7), Observed::BehindFrontier);
    }

    #[test]
    fn recovery_honours_initial_frontier() {
        let initial = PartitionFrontier {
            partition: 4,
            sequence: 100,
        };
        let mut t = FrontierTracker::new(4, Some(initial), Instant::now());
        assert_eq!(t.frontier(), 100);

        // Everything the initial frontier already covers is behind it.
        assert_eq!(t.observe(100), Observed::BehindFrontier);
        assert_eq!(t.observe(50), Observed::BehindFrontier);

        // The first genuinely new sequence holds the frontier where it is until
        // it completes.
        assert_eq!(t.observe(101), Observed::New);
        assert_eq!(t.frontier(), 100);
        assert_eq!(t.complete(101), Advance { from: 100, to: 101 });
    }

    #[test]
    fn due_obeys_the_count_trigger() {
        let t0 = Instant::now();
        let cfg = FrontierConfig {
            every: 3,
            at_least_every: Duration::from_secs(3600),
        };
        let mut t = FrontierTracker::with_config(0, None, t0, cfg);
        for seq in 1..=3 {
            t.observe(seq);
        }

        // One completion, well short of the count and time triggers.
        t.complete(1);
        assert_eq!(t.due(t0), None);

        // Third completion reaches `every`, so a write is due — even though no
        // wall-clock time has passed.
        t.complete(2);
        t.complete(3);
        let due = t.due(t0).expect("count trigger should fire");
        assert_eq!(
            due,
            PartitionFrontier {
                partition: 0,
                sequence: 3,
            }
        );

        // Confirming the write clears the trigger.
        t.persisted(due.sequence, t0);
        assert_eq!(t.due(t0), None);
    }

    #[test]
    fn due_obeys_the_time_trigger() {
        let t0 = Instant::now();
        let cfg = FrontierConfig {
            every: 1_000,
            at_least_every: Duration::from_secs(1),
        };
        let mut t = FrontierTracker::with_config(0, None, t0, cfg);
        t.observe(1);
        t.complete(1);

        // Below the count trigger and within the time window: not yet due.
        assert_eq!(t.due(t0), None);
        assert_eq!(t.due(t0 + Duration::from_millis(999)), None);

        // Once the window elapses the sparse completion is persisted anyway.
        let now = t0 + Duration::from_millis(1_000);
        let due = t.due(now).expect("time trigger should fire");
        assert_eq!(due.sequence, 1);

        t.persisted(due.sequence, now);
        // The window restarts from the confirmation instant.
        assert_eq!(t.due(now + Duration::from_millis(500)), None);
    }

    #[test]
    fn due_requires_the_frontier_to_have_moved() {
        let t0 = Instant::now();
        let cfg = FrontierConfig {
            every: 1,
            at_least_every: Duration::from_millis(0),
        };
        // Recover at 5, so frontier and persisted mark start equal.
        let initial = PartitionFrontier {
            partition: 0,
            sequence: 5,
        };
        let mut t = FrontierTracker::with_config(0, Some(initial), t0, cfg);

        // Observing the next sequence keeps the frontier at 5 (it is the oldest
        // hole minus one), so despite hair-trigger config nothing is due.
        assert_eq!(t.observe(6), Observed::New);
        assert_eq!(t.frontier(), 5);
        assert_eq!(t.due(t0 + Duration::from_secs(10)), None);

        // Only once a completion pushes the frontier past the persisted mark
        // does a write become due.
        assert_eq!(t.complete(6), Advance { from: 5, to: 6 });
        assert_eq!(
            t.due(t0),
            Some(PartitionFrontier {
                partition: 0,
                sequence: 6,
            })
        );
    }
}
