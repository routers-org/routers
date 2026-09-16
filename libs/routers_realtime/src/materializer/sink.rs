//! The materialiser sink: the pure merge core and the trait both backends
//! implement.
//!
//! Several independent consumers apply a
//! [`CommittedOutput`](crate::protocol::output::CommittedOutput), so the decision
//! logic lives here as one pure, idempotent function, [`merge`]. Idempotence lets
//! a sink acknowledge only after it has persisted: a replayed output resolves to
//! [`Applied::Duplicate`].

// The trait exposes a native `async fn`, driven on a single materialiser task
// and never required to be `Send`, so the discouragement does not apply.
#![allow(async_fn_in_trait)]

use alloc::collections::BTreeMap;

use routers_network::Entry;

use crate::event::MatchedLayer;
use crate::protocol::ids::{Revision, SegmentId};
use crate::protocol::output::{CommittedOutput, OutputKind, is_final, supersedes};

/// What applying one output did to the materialised state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// New layers landed at timestamps that held nothing before.
    Inserted {
        /// How many layers were written.
        layers: usize,
    },
    /// A higher revision replaced existing layers, and/or some layers were kept
    /// because they are final.
    Superseded {
        /// How many layers were overwritten by the higher revision.
        layers: usize,
        /// How many layers were left in place because they are final.
        kept_final: usize,
    },
    /// Nothing changed: every layer was an equal or lower revision (a redelivery
    /// or a losing race).
    Duplicate,
    /// Non-final layers were dropped by a retraction.
    Retracted {
        /// How many layers were removed.
        layers: usize,
    },
    /// A reset opened a new segment.
    SegmentOpened,
    /// A terminal output was recorded; it carries no layers.
    Terminal,
}

/// One materialised layer, tagged with the revision that produced it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(bound(
    serialize = "E: serde::Serialize",
    deserialize = "E: serde::Deserialize<'de>"
))]
pub struct StoredLayer<E: Entry> {
    /// The revision that emitted this layer; competing emissions resolve by it.
    pub revision: Revision,
    /// The matched layer itself.
    pub layer: MatchedLayer<E>,
}

/// The materialised state of one continuity segment: its layers keyed by
/// timestamp, the finality watermark, and the highest revision it has seen.
pub struct SegmentState<E: Entry> {
    /// Layers by observation timestamp (microseconds), ordered for range scans.
    pub layers: BTreeMap<i64, StoredLayer<E>>,
    /// Layers at or below this timestamp are final and never rewritten.
    pub finalized_through: Option<i64>,
    /// The highest revision applied to this segment, for observability.
    pub last_revision: Option<Revision>,
}

// Manual `Default` so it does not demand `E: Default`.
impl<E: Entry> Default for SegmentState<E> {
    fn default() -> Self {
        Self {
            layers: BTreeMap::new(),
            finalized_through: None,
            last_revision: None,
        }
    }
}

impl<E: Entry> SegmentState<E> {
    /// An empty segment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// The segment an output applies to: a [`OutputKind::Reset`] targets its
/// `new_segment`, every other kind targets the output's own `segment`.
pub fn target_segment<E: Entry>(output: &CommittedOutput<E>) -> SegmentId {
    match &output.kind {
        OutputKind::Reset { new_segment, .. } => *new_segment,
        _ => output.segment,
    }
}

/// Fold one committed output into a single segment's state, returning what it
/// did. Idempotent under replay. The caller has already selected the correct
/// [`SegmentState`] (see [`target_segment`] and [`VehicleMaterialized::apply`]).
pub fn merge<E: Entry>(state: &mut SegmentState<E>, output: &CommittedOutput<E>) -> Applied {
    match &output.kind {
        OutputKind::Matched {
            diff,
            finalized_through,
        } => {
            // Finality is judged against the watermark *before* this output.
            let watermark = state.finalized_through;
            let revision = output.revision;

            let mut inserted = 0usize;
            let mut superseded = 0usize;
            let mut kept_final = 0usize;

            for layer in &diff.layers {
                let timestamp = layer.timestamp;
                if is_final(timestamp, watermark) {
                    kept_final += 1;
                    continue;
                }
                let existing = state.layers.get(&timestamp).map(|stored| stored.revision);
                if supersedes(revision, existing) {
                    let replacing = existing.is_some();
                    state.layers.insert(
                        timestamp,
                        StoredLayer {
                            revision,
                            layer: layer.clone(),
                        },
                    );
                    if replacing {
                        superseded += 1;
                    } else {
                        inserted += 1;
                    }
                }
            }

            if let Some(through) = *finalized_through {
                state.finalized_through = Some(match state.finalized_through {
                    Some(existing) if existing >= through => existing,
                    _ => through,
                });
            }

            state.last_revision = Some(match state.last_revision {
                Some(current) if current.0 >= revision.0 => current,
                _ => revision,
            });

            let changed = inserted + superseded;
            if superseded > 0 || kept_final > 0 {
                Applied::Superseded {
                    layers: changed,
                    kept_final,
                }
            } else if inserted > 0 {
                Applied::Inserted { layers: inserted }
            } else {
                Applied::Duplicate
            }
        }

        OutputKind::Retraction { timestamps } => {
            let watermark = state.finalized_through;
            let mut removed = 0usize;
            for &timestamp in timestamps {
                // Final layers survive a retraction; only refinable ones drop.
                if !is_final(timestamp, watermark) && state.layers.remove(&timestamp).is_some() {
                    removed += 1;
                }
            }
            Applied::Retracted { layers: removed }
        }

        // A terminal changes no layers; the segment stays as it is.
        OutputKind::Terminal { .. } => Applied::Terminal,

        // The new segment arrives empty and stays empty here.
        OutputKind::Reset { .. } => Applied::SegmentOpened,
    }
}

/// One vehicle's whole materialised history: every segment it has, and which is
/// current. Outputs route by segment, so a late output for an older segment
/// still merges there.
pub struct VehicleMaterialized<E: Entry> {
    /// Every continuity segment this vehicle has, oldest first.
    pub segments: BTreeMap<SegmentId, SegmentState<E>>,
    /// The segment new work belongs to (advanced only by a reset).
    pub current: SegmentId,
}

impl<E: Entry> VehicleMaterialized<E> {
    /// A vehicle whose only (current) segment is `current`, empty.
    #[must_use]
    pub fn new(current: SegmentId) -> Self {
        let mut segments = BTreeMap::new();
        segments.insert(current, SegmentState::new());
        Self { segments, current }
    }

    /// Apply one output, routing it to the segment it belongs to. A reset makes
    /// its `new_segment` current; any other kind merges into `output.segment`,
    /// creating the segment if unseen.
    pub fn apply(&mut self, output: &CommittedOutput<E>) -> Applied {
        let segment = target_segment(output);
        if matches!(output.kind, OutputKind::Reset { .. }) {
            self.current = segment;
        }
        let state = self.segments.entry(segment).or_default();
        merge(state, output)
    }
}

/// A destination for committed output.
///
/// The sink persists an output *before* the consumer acknowledges it, so a
/// crash between apply and ack redelivers the output and the idempotent
/// [`merge`] absorbs the replay.
pub trait Sink<E: Entry>: Send + Sync {
    /// Why an apply failed; surfaced so the consumer can negatively acknowledge.
    type Error: core::error::Error + Send + Sync + 'static;

    /// Durably apply one output to the served view, reporting what it did.
    async fn apply(&self, output: &CommittedOutput<E>) -> Result<Applied, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_network::{DirectionAwareEdgeId, Edge};

    use crate::event::{MatchedDiff, VehicleId};
    use crate::protocol::ids::{JobId, ObservationId};
    use crate::protocol::output::{OutputKind, ResetReason, TerminalReason};

    type E = MockEntryId;

    fn layer(timestamp: i64) -> MatchedLayer<E> {
        MatchedLayer {
            timestamp,
            edge: Edge {
                source: MockEntryId(1),
                target: MockEntryId(2),
                weight: 1,
                id: DirectionAwareEdgeId::new(MockEntryId(3)),
            },
            position: Point::new(151.2, -33.8),
            path: Vec::new(),
        }
    }

    fn matched(
        revision: u64,
        segment: u64,
        timestamps: &[i64],
        finalized_through: Option<i64>,
    ) -> CommittedOutput<E> {
        let diff = MatchedDiff {
            revision,
            downgraded: false,
            layers: timestamps.iter().copied().map(layer).collect(),
        };
        CommittedOutput::new(
            JobId(u128::from(revision)),
            VehicleId(1),
            ObservationId {
                partition: 0,
                sequence: revision,
            },
            Revision(revision),
            SegmentId(segment),
            OutputKind::Matched {
                diff,
                finalized_through,
            },
        )
    }

    fn output_of(revision: u64, segment: u64, kind: OutputKind<E>) -> CommittedOutput<E> {
        CommittedOutput::new(
            JobId(u128::from(revision)),
            VehicleId(1),
            ObservationId {
                partition: 0,
                sequence: revision,
            },
            Revision(revision),
            SegmentId(segment),
            kind,
        )
    }

    #[test]
    fn revision_resolution_table() {
        // (stored revision, incoming revision, expected result, expected stored)
        let cases = [
            (
                5u64,
                7u64,
                Applied::Superseded {
                    layers: 1,
                    kept_final: 0,
                },
                7u64,
            ),
            (5, 5, Applied::Duplicate, 5),
            (7, 5, Applied::Duplicate, 7),
        ];
        for (stored, incoming, expected, kept) in cases {
            let mut state = SegmentState::<E>::new();
            assert_eq!(
                merge(&mut state, &matched(stored, 1, &[100], None)),
                Applied::Inserted { layers: 1 },
                "seed revision {stored}",
            );
            assert_eq!(
                merge(&mut state, &matched(incoming, 1, &[100], None)),
                expected,
                "incoming {incoming} over stored {stored}",
            );
            assert_eq!(
                state.layers[&100].revision,
                Revision(kept),
                "stored revision after {incoming} over {stored}",
            );
        }
    }

    #[test]
    fn fresh_layers_insert() {
        let mut state = SegmentState::<E>::new();
        assert_eq!(
            merge(&mut state, &matched(5, 1, &[100, 200, 300], None)),
            Applied::Inserted { layers: 3 },
        );
        assert_eq!(state.layers.len(), 3);
        assert_eq!(state.last_revision, Some(Revision(5)));
    }

    #[test]
    fn finalized_layers_survive_a_higher_revision() {
        let mut state = SegmentState::<E>::new();
        assert_eq!(
            merge(&mut state, &matched(5, 1, &[100], Some(100))),
            Applied::Inserted { layers: 1 },
        );
        assert_eq!(state.finalized_through, Some(100));

        assert_eq!(
            merge(&mut state, &matched(7, 1, &[100], None)),
            Applied::Superseded {
                layers: 0,
                kept_final: 1,
            },
        );
        assert_eq!(state.layers[&100].revision, Revision(5));
    }

    #[test]
    fn watermark_only_rises() {
        let mut state = SegmentState::<E>::new();
        merge(&mut state, &matched(5, 1, &[100], Some(200)));
        assert_eq!(state.finalized_through, Some(200));
        merge(&mut state, &matched(6, 1, &[300], Some(100)));
        assert_eq!(state.finalized_through, Some(200));
    }

    #[test]
    fn retraction_skips_finals() {
        let mut state = SegmentState::<E>::new();
        // t=100 becomes final; t=200 stays refinable.
        merge(&mut state, &matched(5, 1, &[100], Some(100)));
        merge(&mut state, &matched(5, 1, &[200], None));
        assert_eq!(state.layers.len(), 2);

        let retraction = output_of(
            6,
            1,
            OutputKind::Retraction {
                timestamps: vec![100, 200],
            },
        );
        assert_eq!(
            merge(&mut state, &retraction),
            Applied::Retracted { layers: 1 }
        );
        assert!(state.layers.contains_key(&100), "final layer survives");
        assert!(!state.layers.contains_key(&200), "non-final layer dropped");
    }

    #[test]
    fn terminal_changes_nothing() {
        let mut state = SegmentState::<E>::new();
        merge(&mut state, &matched(5, 1, &[100], None));
        let terminal = output_of(
            6,
            1,
            OutputKind::Terminal {
                reason: TerminalReason::Unanchored,
                closes_segment: false,
            },
        );
        assert_eq!(merge(&mut state, &terminal), Applied::Terminal);
        assert_eq!(state.layers.len(), 1);
    }

    #[test]
    fn reset_opens_a_segment_and_old_stays() {
        let mut vehicle = VehicleMaterialized::<E>::new(SegmentId(1));
        assert_eq!(
            vehicle.apply(&matched(5, 1, &[100], Some(100))),
            Applied::Inserted { layers: 1 },
        );

        let reset = output_of(
            6,
            1,
            OutputKind::Reset {
                reason: ResetReason::Gap,
                new_segment: SegmentId(2),
            },
        );
        assert_eq!(vehicle.apply(&reset), Applied::SegmentOpened);
        assert_eq!(vehicle.current, SegmentId(2));

        assert_eq!(
            vehicle.apply(&matched(7, 2, &[200], None)),
            Applied::Inserted { layers: 1 },
        );

        let old = &vehicle.segments[&SegmentId(1)];
        assert_eq!(old.layers.len(), 1, "old segment keeps its layer");
        assert_eq!(old.layers[&100].revision, Revision(5));
        assert_eq!(old.finalized_through, Some(100));

        let new = &vehicle.segments[&SegmentId(2)];
        assert_eq!(new.layers.len(), 1, "new segment holds the fresh layer");
        assert!(new.layers.contains_key(&200));
    }

    #[test]
    fn late_output_for_an_older_segment_merges_there() {
        let mut vehicle = VehicleMaterialized::<E>::new(SegmentId(1));
        vehicle.apply(&matched(5, 1, &[100], None));
        let reset = output_of(
            6,
            1,
            OutputKind::Reset {
                reason: ResetReason::Gap,
                new_segment: SegmentId(2),
            },
        );
        vehicle.apply(&reset);
        assert_eq!(vehicle.current, SegmentId(2));

        assert_eq!(
            vehicle.apply(&matched(4, 1, &[50], None)),
            Applied::Inserted { layers: 1 },
        );
        assert_eq!(vehicle.current, SegmentId(2));
        assert!(vehicle.segments[&SegmentId(1)].layers.contains_key(&50));
    }

    #[test]
    fn target_segment_reads_reset_new_segment() {
        let reset = output_of(
            6,
            1,
            OutputKind::Reset {
                reason: ResetReason::Operator,
                new_segment: SegmentId(9),
            },
        );
        assert_eq!(target_segment(&reset), SegmentId(9));

        let plain = matched(5, 3, &[100], None);
        assert_eq!(target_segment(&plain), SegmentId(3));
    }
}
