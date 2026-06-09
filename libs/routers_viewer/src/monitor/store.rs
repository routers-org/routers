use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use geo::Point;
use routers_realtime::context::{MatchContext, MatchOutcome, MatchResult};
use routers_shard::Geohash;

const TRACE_WINDOW: usize = 50;
const PENDING_TTL_MS: u128 = 5_000;

#[derive(Clone)]
pub(crate) struct VehicleFix {
    pub raw_coord: Point,
    pub matched_coord: Option<Point>,
    pub outcome: MatchOutcome,
}

struct PendingEntry {
    raw_coord: Option<Point>,
    result: Option<MatchResult>,
    inserted_at: Instant,
}

pub(crate) struct StoreStats {
    pub vehicle_count: usize,
    pub events_per_sec: f32,
    pub success: u64,
    pub no_candidate: u64,
    pub error: u64,
}

pub(crate) struct VehicleTraceStore {
    pending: HashMap<(String, u64), PendingEntry>,
    pub traces: HashMap<String, VecDeque<VehicleFix>>,
    pub active_shards: HashSet<Geohash>,
    event_bucket: VecDeque<Instant>,
    success: u64,
    no_candidate: u64,
    error: u64,
}

impl VehicleTraceStore {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            traces: HashMap::new(),
            active_shards: HashSet::new(),
            event_bucket: VecDeque::new(),
            success: 0,
            no_candidate: 0,
            error: 0,
        }
    }

    pub fn ingest_context(&mut self, ctx: MatchContext<Geohash>) {
        self.active_shards.insert(ctx.target_shard);
        let key = (ctx.vehicle_id.clone(), ctx.resolved_at_ms);
        let raw_coord = ctx.current.coord;
        self.pending
            .entry(key.clone())
            .or_insert_with(|| PendingEntry {
                raw_coord: None,
                result: None,
                inserted_at: Instant::now(),
            })
            .raw_coord = Some(raw_coord);
        self.try_commit(&key);
    }

    pub fn ingest_result(&mut self, result: MatchResult) {
        let key = (result.vehicle_id.clone(), result.resolved_at_ms);
        self.pending
            .entry(key.clone())
            .or_insert_with(|| PendingEntry {
                raw_coord: None,
                result: None,
                inserted_at: Instant::now(),
            })
            .result = Some(result);
        self.try_commit(&key);
    }

    fn try_commit(&mut self, key: &(String, u64)) {
        let (raw_coord, outcome, coord) = {
            let Some(entry) = self.pending.get(key) else {
                return;
            };
            match (entry.raw_coord, entry.result.as_ref()) {
                (Some(raw), Some(result)) => (raw, result.outcome, result.coord),
                _ => return,
            }
        };

        let fix = VehicleFix {
            raw_coord,
            matched_coord: (outcome == MatchOutcome::Success).then_some(coord),
            outcome,
        };

        match outcome {
            MatchOutcome::Success => self.success += 1,
            MatchOutcome::NoCandidate => self.no_candidate += 1,
            MatchOutcome::Error => self.error += 1,
        }

        let trace = self.traces.entry(key.0.clone()).or_default();
        if trace.len() >= TRACE_WINDOW {
            trace.pop_front();
        }
        trace.push_back(fix);
        self.event_bucket.push_back(Instant::now());
        self.pending.remove(key);
    }

    pub fn evict_stale(&mut self) {
        let now = Instant::now();
        self.pending
            .retain(|_, e| now.duration_since(e.inserted_at).as_millis() < PENDING_TTL_MS);
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
            .filter(|t| now.duration_since(**t).as_secs_f64() < 1.0)
            .count();
        StoreStats {
            vehicle_count: self.traces.len(),
            events_per_sec: recent as f32,
            success: self.success,
            no_candidate: self.no_candidate,
            error: self.error,
        }
    }
}
