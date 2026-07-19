use alloc::collections::VecDeque;
use core::time::Duration;
use std::collections::HashMap;
use web_time::Instant;

use geo::Point;
use routers_realtime::event::MatchResult;

use crate::{E, M};

/// A vehicle's latest full matched solution, oldest point first. Each new
/// result supersedes the previous one entirely.
pub struct VehicleTrace {
    pub segments: Vec<Point>,
    pub last_seen: Instant,
}

impl VehicleTrace {
    fn new() -> Self {
        Self {
            segments: Vec::new(),
            last_seen: Instant::now(),
        }
    }

    fn assign(&mut self, mut segment: Vec<Point>, capacity: usize) {
        // Keep only the newest `capacity` points; the segment runs
        // oldest→newest, so trim from the front.
        if segment.len() > capacity {
            segment.drain(..segment.len() - capacity);
        }

        self.segments = segment;
        self.last_seen = Instant::now();
    }

    /// The full tail as one point sequence, for rendering.
    pub fn flattened(&self) -> Vec<Point> {
        self.segments.clone()
    }
}

pub struct StoreStats {
    pub vehicle_count: usize,
    pub events_per_sec: usize,
    pub total_events: u64,
}

/// Latest matched solution per vehicle. Memory is bounded on both axes:
/// each vehicle retains at most `capacity` points, and vehicles that go
/// quiet for longer than `idle_ttl` are evicted entirely.
pub struct TraceStore {
    capacity: usize,
    idle_ttl: Duration,
    pub traces: HashMap<String, VehicleTrace>,
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

    pub fn ingest(&mut self, result: MatchResult<E, M>) {
        let now = Instant::now();

        self.event_bucket.push_back(now);
        self.total_events += 1;

        // The matched solution arrives in chronological order, so the last
        // point is the vehicle's current position, which the plugin marks
        // with the head dot.
        let segment: Vec<Point> = result
            .path
            .interpolated
            .iter()
            .map(|element| Point::from(element.point))
            .collect();

        if segment.is_empty() {
            return;
        }

        self.traces
            .entry(result.vehicle_id)
            .or_insert_with(VehicleTrace::new)
            .assign(segment, self.capacity);
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
