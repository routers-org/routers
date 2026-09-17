use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use core::time::Duration;
use std::collections::{HashMap, HashSet};
use web_time::Instant;

use geo::Point;
use routers_realtime::event::{MatchedDiff, VehicleId};
use routers_realtime::protocol::ids::SegmentId;
use routers_realtime::protocol::output::supersedes;
use routers_realtime::protocol::{CommittedOutput, OutputKind, Revision};

use crate::E;

/// One segment of a vehicle's matched history, as the orchestrator currently
/// resolves it: a finalized prefix plus the live window of the latest commit.
pub struct SegmentTrace {
    /// Observation timestamp → `(revision that set it, its geometry)`.
    layers: BTreeMap<i64, (Revision, Vec<Point>)>,
    finalized_through: Option<i64>,
    pub last_seen: Instant,
}

fn is_final(timestamp: i64, finalized_through: Option<i64>) -> bool {
    finalized_through.is_some_and(|watermark| timestamp <= watermark)
}

impl SegmentTrace {
    fn new() -> Self {
        Self {
            layers: BTreeMap::new(),
            finalized_through: None,
            last_seen: Instant::now(),
        }
    }

    /// Replace the live window with this commit's diff. A live layer the diff no
    /// longer covers was re-matched away, so it goes; final layers stay.
    fn resolve(
        &mut self,
        revision: Revision,
        diff: &MatchedDiff<E>,
        finalized_through: Option<i64>,
        capacity: usize,
    ) {
        let watermark = self.finalized_through;
        let covered: BTreeSet<i64> = diff.layers.iter().map(|layer| layer.timestamp).collect();
        self.layers
            .retain(|timestamp, _| is_final(*timestamp, watermark) || covered.contains(timestamp));

        for layer in &diff.layers {
            let existing = self.layers.get(&layer.timestamp).map(|(rev, _)| *rev);
            if supersedes(revision, existing) {
                let mut segment = layer.path.clone();
                segment.push(layer.position);
                self.layers.insert(layer.timestamp, (revision, segment));
            }
        }

        self.finalized_through = match (self.finalized_through, finalized_through) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };

        while self.layers.len() > capacity {
            self.layers.pop_first();
        }

        self.touch();
    }

    fn retract(&mut self, timestamps: &[i64]) {
        for timestamp in timestamps {
            self.layers.remove(timestamp);
        }
        self.touch();
    }

    fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    /// The segment as one point sequence, oldest observation first.
    pub fn flattened(&self) -> Vec<Point> {
        self.layers
            .values()
            .flat_map(|(_, points)| points)
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

        let key = (output.vehicle_id, output.segment);
        match &output.kind {
            OutputKind::Matched {
                diff,
                finalized_through,
            } => {
                self.traces
                    .entry(key)
                    .or_insert_with(SegmentTrace::new)
                    .resolve(output.revision, diff, *finalized_through, self.capacity);
            }
            OutputKind::Retraction { timestamps } => {
                if let Some(trace) = self.traces.get_mut(&key) {
                    trace.retract(timestamps);
                }
            }
            OutputKind::Terminal { .. } | OutputKind::Reset { .. } => {
                if let Some(trace) = self.traces.get_mut(&key) {
                    trace.touch();
                }
            }
        }
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
