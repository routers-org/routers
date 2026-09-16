use alloc::collections::{BTreeMap, VecDeque};
use core::time::Duration;
use std::collections::HashMap;
use web_time::Instant;

use geo::Point;
use routers_realtime::event::{MatchedDiff, VehicleId};
use routers_realtime::protocol::output::supersedes;
use routers_realtime::protocol::{CommittedOutput, OutputKind, Revision};

use crate::E;

/// A vehicle's matched history, merged from committed outputs: one geometry
/// segment per observation timestamp, kept by highest revision so a stale
/// re-delivery never clobbers a newer layer.
pub struct VehicleTrace {
    /// Observation timestamp → `(revision that set it, its geometry)`.
    layers: BTreeMap<i64, (Revision, Vec<Point>)>,
    pub last_seen: Instant,
}

impl VehicleTrace {
    fn new() -> Self {
        Self {
            layers: BTreeMap::new(),
            last_seen: Instant::now(),
        }
    }

    /// Merge a diff, keeping the highest revision per timestamp.
    fn merge(&mut self, revision: Revision, diff: &MatchedDiff<E>, capacity: usize) {
        for layer in &diff.layers {
            let existing = self.layers.get(&layer.timestamp).map(|(rev, _)| *rev);
            if supersedes(revision, existing) {
                let mut segment = layer.path.clone();
                segment.push(layer.position);
                self.layers.insert(layer.timestamp, (revision, segment));
            }
        }

        // Bound by observation count, trimming the oldest.
        while self.layers.len() > capacity {
            self.layers.pop_first();
        }

        self.touch();
    }

    /// Drop the retracted (non-final) timestamps.
    fn retract(&mut self, timestamps: &[i64]) {
        for timestamp in timestamps {
            self.layers.remove(timestamp);
        }
        self.touch();
    }

    fn touch(&mut self) {
        self.last_seen = Instant::now();
    }

    /// The full tail as one point sequence, oldest observation first.
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

/// Merged matched history per vehicle. Memory is bounded on both axes:
/// each vehicle retains at most `capacity` observations, and vehicles that
/// go quiet for longer than `idle_ttl` are evicted entirely.
pub struct TraceStore {
    capacity: usize,
    idle_ttl: Duration,
    pub traces: HashMap<VehicleId, VehicleTrace>,
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

        match &output.kind {
            OutputKind::Matched { diff, .. } => {
                self.traces
                    .entry(output.vehicle_id)
                    .or_insert_with(VehicleTrace::new)
                    .merge(output.revision, diff, self.capacity);
            }
            OutputKind::Retraction { timestamps } => {
                if let Some(trace) = self.traces.get_mut(&output.vehicle_id) {
                    trace.retract(timestamps);
                }
            }
            // Resets and terminals only keep a known vehicle alive against idle eviction.
            OutputKind::Terminal { .. } | OutputKind::Reset { .. } => {
                if let Some(trace) = self.traces.get_mut(&output.vehicle_id) {
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

        StoreStats {
            vehicle_count: self.traces.len(),
            events_per_sec: recent,
            total_events: self.total_events,
        }
    }
}
