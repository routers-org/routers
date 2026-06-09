use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use geo::{Coord, Point};
use routers_realtime::context::{MatchContext, MatchOutcome, MatchResult, MatchRoute};
use routers_shard::Geohash;

/// How many raw GPS fixes to keep per vehicle — just enough to show snap lines.
const TRACE_WINDOW: usize = 20;
/// Drop a route if no update has arrived within this many seconds.
const ROUTE_TTL_SECS: f32 = 15.0;
const PENDING_TTL_MS: u128 = 5_000;
/// How long a corrected fix glows before fading back to normal.
pub(crate) const CORRECTION_FLASH_SECS: f32 = 2.0;

#[derive(Clone)]
pub(crate) struct VehicleFix {
    pub resolved_at_ms: u64,
    pub raw_coord: Point,
    pub matched_coord: Option<Point>,
    pub outcome: MatchOutcome,
    /// Set when the HMM revised this fix after initially publishing it.
    pub last_corrected_at: Option<Instant>,
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
    pub corrections: u64,
}

pub(crate) struct VehicleTraceStore {
    pending: HashMap<(String, u64), PendingEntry>,
    pub traces: HashMap<String, VecDeque<VehicleFix>>,
    /// Latest interpolated road polyline per vehicle, with the time it was last updated.
    pub routes: HashMap<String, (Vec<Coord>, Instant)>,
    pub active_shards: std::collections::HashSet<Geohash>,
    event_bucket: VecDeque<Instant>,
    success: u64,
    no_candidate: u64,
    error: u64,
    corrections: u64,
}

impl VehicleTraceStore {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            traces: HashMap::new(),
            routes: HashMap::new(),
            active_shards: std::collections::HashSet::new(),
            event_bucket: VecDeque::new(),
            success: 0,
            no_candidate: 0,
            error: 0,
            corrections: 0,
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

        // Primary path: join with a pending context entry (current event).
        if self.pending.contains_key(&key) {
            self.pending
                .entry(key.clone())
                .or_insert_with(|| PendingEntry {
                    raw_coord: None,
                    result: None,
                    inserted_at: Instant::now(),
                })
                .result = Some(result);
            self.try_commit(&key);
            return;
        }

        // Secondary path: the result is for a historical GPS point that was
        // already committed in a prior window. Update it in-place.
        // (The correction path is preferred for explicit corrections but this
        // handles the normal full-window re-emission without a coord change.)
        self.apply_update(&result.vehicle_id, result.resolved_at_ms, result.coord, false);
    }

    pub fn ingest_route(&mut self, route: MatchRoute) {
        self.routes.insert(route.vehicle_id, (route.polyline, Instant::now()));
    }

    pub fn ingest_correction(&mut self, result: MatchResult) {
        self.apply_update(&result.vehicle_id, result.resolved_at_ms, result.coord, true);
    }

    /// Update a committed fix in-place. `is_correction` triggers the flash animation.
    fn apply_update(&mut self, vehicle_id: &str, resolved_at_ms: u64, new_coord: Point, is_correction: bool) {
        let Some(trace) = self.traces.get_mut(vehicle_id) else { return };
        for fix in trace.iter_mut() {
            if fix.resolved_at_ms == resolved_at_ms {
                fix.matched_coord = Some(new_coord);
                if is_correction {
                    fix.last_corrected_at = Some(Instant::now());
                    self.corrections += 1;
                }
                return;
            }
        }
    }

    fn try_commit(&mut self, key: &(String, u64)) {
        let (raw_coord, outcome, coord) = {
            let Some(entry) = self.pending.get(key) else { return };
            match (entry.raw_coord, entry.result.as_ref()) {
                (Some(raw), Some(result)) => (raw, result.outcome, result.coord),
                _ => return,
            }
        };

        let fix = VehicleFix {
            resolved_at_ms: key.1,
            raw_coord,
            matched_coord: (outcome == MatchOutcome::Success).then_some(coord),
            outcome,
            last_corrected_at: None,
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
        // Remove routes that haven't been updated recently — vehicle went quiet.
        self.routes.retain(|_, (_, updated_at)| {
            now.duration_since(*updated_at).as_secs_f32() < ROUTE_TTL_SECS
        });
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
            corrections: self.corrections,
        }
    }
}
