//! An in-memory [`Sink`]: the materialised view held in a map.
//!
//! [`MemorySink`] lets a test drive the real consumer and merge logic and read
//! back what a downstream reader would see, without a store. Cheap to [`Clone`]:
//! every clone shares the same map through an `Arc`.

use alloc::sync::Arc;
use core::convert::Infallible;
use std::collections::HashMap;
use std::sync::Mutex;

use routers_network::Entry;

use crate::event::VehicleId;
use crate::materializer::sink::{Applied, Sink, VehicleMaterialized, target_segment};
use crate::protocol::output::CommittedOutput;

/// An in-memory materialised view keyed by vehicle. Applying an output is
/// infallible, so a consumer over this sink always acknowledges.
pub struct MemorySink<E: Entry> {
    vehicles: Arc<Mutex<HashMap<VehicleId, VehicleMaterialized<E>>>>,
}

// Manual `Clone` so a clone shares the map without demanding `E: Clone`.
impl<E: Entry> Clone for MemorySink<E> {
    fn clone(&self) -> Self {
        Self {
            vehicles: Arc::clone(&self.vehicles),
        }
    }
}

impl<E: Entry> Default for MemorySink<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Entry> MemorySink<E> {
    /// A fresh, empty view.
    #[must_use]
    pub fn new() -> Self {
        Self {
            vehicles: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A snapshot of one vehicle's materialised history, or `None` if it has no
    /// output yet.
    #[must_use]
    pub fn snapshot(&self, vehicle: VehicleId) -> Option<VehicleMaterialized<E>> {
        self.vehicles.lock().unwrap().get(&vehicle).cloned()
    }

    /// How many vehicles the view holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vehicles.lock().unwrap().len()
    }

    /// Whether the view is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vehicles.lock().unwrap().is_empty()
    }
}

impl<E: Entry> Sink<E> for MemorySink<E> {
    type Error = Infallible;

    async fn apply(&self, output: &CommittedOutput<E>) -> Result<Applied, Infallible> {
        // Synchronous critical section: the guard never crosses a suspend point.
        let mut vehicles = self.vehicles.lock().unwrap();
        let vehicle = vehicles
            .entry(output.vehicle_id)
            .or_insert_with(|| VehicleMaterialized::new(target_segment(output)));
        Ok(vehicle.apply(output))
    }
}

// Manual `Clone` for `snapshot`, avoiding an `E: Clone` bound.
impl<E: Entry> Clone for VehicleMaterialized<E> {
    fn clone(&self) -> Self {
        Self {
            segments: self.segments.clone(),
            current: self.current,
        }
    }
}

impl<E: Entry> Clone for crate::materializer::sink::SegmentState<E> {
    fn clone(&self) -> Self {
        Self {
            layers: self.layers.clone(),
            finalized_through: self.finalized_through,
            last_revision: self.last_revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_network::{DirectionAwareEdgeId, Edge};

    use crate::event::{MatchedDiff, MatchedLayer};
    use crate::materializer::sink::Applied;
    use crate::protocol::ids::{JobId, ObservationId, Revision, SegmentId};
    use crate::protocol::output::OutputKind;

    type E = MockEntryId;

    fn matched(vehicle: u64, revision: u64, timestamp: i64) -> CommittedOutput<E> {
        let diff = MatchedDiff {
            revision,
            downgraded: false,
            layers: vec![MatchedLayer {
                timestamp,
                edge: Edge {
                    source: MockEntryId(1),
                    target: MockEntryId(2),
                    weight: 1,
                    id: DirectionAwareEdgeId::new(MockEntryId(3)),
                },
                position: Point::new(0.0, 0.0),
                path: Vec::new(),
            }],
        };
        CommittedOutput::new(
            JobId(u128::from(revision)),
            VehicleId(vehicle),
            ObservationId {
                partition: 0,
                sequence: revision,
            },
            Revision(revision),
            SegmentId(1),
            OutputKind::Matched {
                diff,
                finalized_through: None,
            },
        )
    }

    #[tokio::test]
    async fn apply_builds_a_per_vehicle_view() {
        let sink = MemorySink::<E>::new();
        assert!(sink.is_empty());

        assert_eq!(
            sink.apply(&matched(1, 5, 100)).await.unwrap(),
            Applied::Inserted { layers: 1 },
        );
        assert_eq!(
            sink.apply(&matched(2, 5, 100)).await.unwrap(),
            Applied::Inserted { layers: 1 },
        );
        assert_eq!(sink.len(), 2);

        let view = sink.snapshot(VehicleId(1)).expect("vehicle present");
        assert_eq!(view.current, SegmentId(1));
        assert_eq!(
            view.segments[&SegmentId(1)].layers[&100].revision,
            Revision(5)
        );

        assert!(sink.snapshot(VehicleId(99)).is_none());
    }

    #[tokio::test]
    async fn replay_is_a_duplicate() {
        let sink = MemorySink::<E>::new();
        sink.apply(&matched(1, 5, 100)).await.unwrap();
        assert_eq!(
            sink.apply(&matched(1, 5, 100)).await.unwrap(),
            Applied::Duplicate,
        );
    }

    #[tokio::test]
    async fn clones_share_one_view() {
        let sink = MemorySink::<E>::new();
        let other = sink.clone();
        sink.apply(&matched(1, 5, 100)).await.unwrap();
        assert!(other.snapshot(VehicleId(1)).is_some());
    }
}
