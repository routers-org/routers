use alloc::collections::VecDeque;
use core::time::Duration;
use std::collections::{HashMap, HashSet};
use web_time::Instant;

use geo::Point;
use routers_realtime::event::VehicleId;
use routers_realtime::materializer::{SegmentState, merge, target_segment};
use routers_realtime::protocol::CommittedOutput;
use routers_realtime::protocol::ids::SegmentId;

use crate::E;

/// One segment of a vehicle's matched history. The shared materializer reducer
/// establishes its finality, revision, and retraction semantics; this viewer
/// only applies its local rendering-capacity bound afterwards.
pub struct SegmentTrace {
    state: SegmentState<E>,
    pub last_seen: Instant,
}

impl SegmentTrace {
    fn new() -> Self {
        Self {
            state: SegmentState::new(),
            last_seen: Instant::now(),
        }
    }

    fn apply(&mut self, output: &CommittedOutput<E>, capacity: usize) {
        merge(&mut self.state, output);
        while self.state.layers.len() > capacity {
            self.state.layers.pop_first();
        }
        self.touch();
    }

    fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    /// The segment as one point sequence, oldest observation first.
    pub fn flattened(&self) -> Vec<Point> {
        self.state
            .layers
            .values()
            .flat_map(|stored| stored.layer.path.iter().chain([&stored.layer.position]))
            .copied()
            .collect()
    }
}

pub struct StoreStats {
    pub vehicle_count: usize,
    pub events_per_sec: usize,
    pub total_events: u64,
}

/// Resolved traces per `(vehicle, segment)`. A reset opens a new segment, so
/// the line breaks instead of jumping. Memory is bounded on both axes: each
/// segment keeps at most `capacity` observations, and segments quiet for
/// longer than `idle_ttl` are evicted.
pub struct TraceStore {
    capacity: usize,
    idle_ttl: Duration,
    pub traces: HashMap<(VehicleId, SegmentId), SegmentTrace>,
    event_bucket: VecDeque<Instant>,
    total_events: u64,
}

impl TraceStore {
    pub fn new(capacity: usize, idle_ttl: Duration) -> Self {
        Self {
            capacity,
            idle_ttl,
            traces: HashMap::new(),
            event_bucket: VecDeque::new(),
            total_events: 0,
        }
    }

    pub fn ingest(&mut self, output: CommittedOutput<E>) {
        let now = Instant::now();

        self.event_bucket.push_back(now);
        self.total_events += 1;

        let key = (output.vehicle_id, target_segment(&output));
        self.traces
            .entry(key)
            .or_insert_with(SegmentTrace::new)
            .apply(&output, self.capacity);
    }

    pub fn evict_idle(&mut self) {
        let now = Instant::now();

        self.traces
            .retain(|_, trace| now.duration_since(trace.last_seen) < self.idle_ttl);

        while self
            .event_bucket
            .front()
            .is_some_and(|t| now.duration_since(*t).as_secs() >= 2)
        {
            self.event_bucket.pop_front();
        }
    }

    pub fn stats(&self) -> StoreStats {
        let now = Instant::now();
        let recent = self
            .event_bucket
            .iter()
            .filter(|t| now.duration_since(**t) < Duration::from_secs(1))
            .count();
        let vehicles: HashSet<VehicleId> =
            self.traces.keys().map(|(vehicle, _)| *vehicle).collect();

        StoreStats {
            vehicle_count: vehicles.len(),
            events_per_sec: recent,
            total_events: self.total_events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use geo::Point;
    use routers_codec::osm::OsmEntryId;
    use routers_network::{DirectionAwareEdgeId, Edge};
    use routers_realtime::event::MatchedLayer;
    use routers_realtime::protocol::ids::{JobId, ObservationId, Revision};
    use routers_realtime::protocol::output::{OutputKind, ResetReason};

    fn output(revision: u64, segment: u64, kind: OutputKind<E>) -> CommittedOutput<E> {
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

    fn matched(revision: u64, segment: u64, timestamp: i64) -> CommittedOutput<E> {
        output(
            revision,
            segment,
            OutputKind::Matched {
                diff: routers_realtime::event::MatchedDiff {
                    revision,
                    downgraded: false,
                    layers: vec![MatchedLayer {
                        timestamp,
                        edge: Edge {
                            source: OsmEntryId::node(1),
                            target: OsmEntryId::node(2),
                            weight: 1,
                            id: DirectionAwareEdgeId::new(OsmEntryId::node(3)),
                        },
                        position: Point::new(151.2, -33.8),
                        path: Vec::new(),
                    }],
                },
                finalized_through: None,
            },
        )
    }

    #[test]
    fn trace_store_uses_the_materializer_reducer_for_tombstones_and_resets() {
        let mut store = TraceStore::new(10, Duration::from_secs(60));
        store.ingest(matched(7, 1, 100));
        store.ingest(output(
            8,
            1,
            OutputKind::Retraction {
                timestamps: vec![100],
            },
        ));
        // The shared reducer's tombstone prevents an out-of-order stale match
        // from reviving a path the viewer had already removed.
        store.ingest(matched(7, 1, 100));

        let trace = &store.traces[&(VehicleId(1), SegmentId(1))];
        assert!(trace.flattened().is_empty());

        // Reset output carries its destination separately from `output.segment`.
        store.ingest(output(
            9,
            1,
            OutputKind::Reset {
                reason: ResetReason::Gap,
                new_segment: SegmentId(2),
            },
        ));
        assert!(store.traces.contains_key(&(VehicleId(1), SegmentId(2))));
    }
}
