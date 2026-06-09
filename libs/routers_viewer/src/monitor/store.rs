use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use geo::{Coord, Point};
use routers_realtime::context::{MatchContext, MatchOutcome, MatchResult, MatchRoute};
use routers_shard::Geohash;

const TRACE_WINDOW: usize = 5;
const ROUTE_TTL_SECS: f32 = 15.0;
/// How long to buffer a result whose context fix hasn't arrived yet.
const PENDING_RESULT_TTL_MS: u128 = 10_000;

#[derive(Clone)]
pub(crate) struct VehicleFix {
    pub resolved_at_ms: u64,
    pub raw_coord: Point,
    pub matched_coord: Option<Point>,
    pub outcome: MatchOutcome,
    pub committed_at: Instant,
}

pub(crate) struct StoreStats {
    pub vehicle_count: usize,
    pub events_per_sec: f32,
    pub success: u64,
    pub no_candidate: u64,
    pub error: u64,
}

pub(crate) struct VehicleTraceStore {
    /// Results that arrived before their GPS context fix was committed.
    pending_results: HashMap<(String, u64), (MatchResult, Instant)>,
    pub traces: HashMap<String, VecDeque<VehicleFix>>,
    pub routes: HashMap<String, (Vec<Coord>, Instant)>,
    pub active_shards: std::collections::HashSet<Geohash>,
    event_bucket: VecDeque<Instant>,
    success: u64,
    no_candidate: u64,
    error: u64,
}

impl VehicleTraceStore {
    pub fn new() -> Self {
        Self {
            pending_results: HashMap::new(),
            traces: HashMap::new(),
            routes: HashMap::new(),
            active_shards: std::collections::HashSet::new(),
            event_bucket: VecDeque::new(),
            success: 0,
            no_candidate: 0,
            error: 0,
        }
    }

    /// Context arrives → commit raw GPS dot immediately so the leading dot always advances.
    /// If a result arrived earlier (result-before-context race), apply it now.
    pub fn ingest_context(&mut self, ctx: MatchContext<Geohash>) {
        self.active_shards.insert(ctx.target_shard);

        let key = (ctx.vehicle_id.clone(), ctx.resolved_at_ms);
        let raw_coord = ctx.current.coord;

        // Check for a buffered result that beat the context here.
        let (matched_coord, outcome) = if let Some((result, _)) = self.pending_results.remove(&key) {
            let outcome = result.outcome;
            let coord = result.coord;
            match outcome {
                MatchOutcome::Success => self.success += 1,
                MatchOutcome::NoCandidate => self.no_candidate += 1,
                MatchOutcome::Error => self.error += 1,
            }
            ((outcome == MatchOutcome::Success).then_some(coord), outcome)
        } else {
            // Result not yet received; the dot shows the raw GPS position immediately.
            // ingest_result will fill in matched_coord when it arrives.
            (None, MatchOutcome::NoCandidate)
        };

        let fix = VehicleFix {
            resolved_at_ms: ctx.resolved_at_ms,
            raw_coord,
            matched_coord,
            outcome,
            committed_at: Instant::now(),
        };

        let trace = self.traces.entry(ctx.vehicle_id).or_default();
        if trace.len() >= TRACE_WINDOW {
            trace.pop_front();
        }
        trace.push_back(fix);
        self.event_bucket.push_back(Instant::now());
    }

    /// Result arrives → find the already-committed fix and fill in the matched position.
    /// If the context hasn't arrived yet, buffer the result briefly.
    pub fn ingest_result(&mut self, result: MatchResult) {
        if let Some(trace) = self.traces.get_mut(&result.vehicle_id) {
            for fix in trace.iter_mut().rev() {
                if fix.resolved_at_ms == result.resolved_at_ms {
                    fix.matched_coord =
                        (result.outcome == MatchOutcome::Success).then_some(result.coord);
                    fix.outcome = result.outcome;
                    match result.outcome {
                        MatchOutcome::Success => self.success += 1,
                        MatchOutcome::NoCandidate => self.no_candidate += 1,
                        MatchOutcome::Error => self.error += 1,
                    }
                    return;
                }
            }
        }

        // Context hasn't landed yet — keep the result around briefly.
        self.pending_results.insert(
            (result.vehicle_id.clone(), result.resolved_at_ms),
            (result, Instant::now()),
        );
    }

    pub fn ingest_route(&mut self, route: MatchRoute) {
        self.routes.insert(route.vehicle_id, (route.polyline, Instant::now()));
    }

    pub fn evict_stale(&mut self) {
        let now = Instant::now();

        self.pending_results
            .retain(|_, (_, t)| now.duration_since(*t).as_millis() < PENDING_RESULT_TTL_MS);

        self.routes.retain(|_, (_, updated_at)| {
            now.duration_since(*updated_at).as_secs_f32() < ROUTE_TTL_SECS
        });

        let trace_ttl_ms = ((ROUTE_TTL_SECS + 2.0) * 1_000.0) as u128;
        self.traces.retain(|_, fixes| {
            fixes
                .back()
                .map_or(false, |f| now.duration_since(f.committed_at).as_millis() < trace_ttl_ms)
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
        }
    }
}
