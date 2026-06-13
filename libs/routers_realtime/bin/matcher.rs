use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use geo::{Distance, Haversine, LineString, Point};
use routers::transition::r#match::DEFAULT_SEARCH_DISTANCE;
use routers::transition::streaming::{MatchState, StreamingMatcher};
use routers::transition::{CostingStrategies, MatchError, RoutedPath};
use routers_network::DirectionAwareEdgeId;
use routers_network::traits::metadata::Metadata;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_realtime::{
    context::{MatchContext, MatchOutcome, MatchResult, MatchRoute},
    metrics,
    nats,
};
#[cfg(not(debug_assertions))]
use routers_realtime::assignment::ShardAssignment;
use routers_shard::{
    FileFetcher, Geohash, GeohashStrategy, Selection, SelectionMode, ShardLoader,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Phase 1A profiling: sample matcher-side timing breakdown every Nth
/// event. Set `MATCH_PROFILE_SAMPLE_N=50` to log 1 in 50 events.
/// Default 0 = disabled, no overhead beyond a relaxed atomic.
static MATCH_PROFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn match_profile_sample_n() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("MATCH_PROFILE_SAMPLE_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

/// Per-vehicle Viterbi frontier cache. `MatchState` is a multi-candidate
/// column under Phase 1C — top-K pruning is applied at writeback when
/// `MATCH_FRONTIER_K` is set.
type StateCache = Arc<DashMap<String, MatchState<OsmEntryId>>>;

type E = OsmEntryId;
type M = OsmEdgeMetadata;
type S = Geohash;
type SolveResult = (
    String,
    u64,
    geo::Point,
    Vec<String>,
    Result<(RoutedPath<E, M>, MatchState<E>), MatchError>,
    f64,
    Instant,
    bool,                              // was_warm — true if warm step over a saved frontier was used
    Instant,                           // t_spawn_at
    Option<DirectionAwareEdgeId<E>>,   // prev argmin edge id (Some only when was_warm) — drives argmin_revisions metric
);

fn frontier_k() -> Option<usize> {
    static CACHED: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("MATCH_FRONTIER_K")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&k: &usize| k > 0)
    })
}

fn shard_filename(key: &Geohash) -> String {
    format!("{}.shard.rt", key)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
    let shard_dir = std::env::var("SHARD_DIR").unwrap_or_else(|_| "./shards".into());
    let shard_precision: u8 = std::env::var("SHARD_PRECISION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let metrics_addr: std::net::SocketAddr = std::env::var("METRICS_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9092".into())
        .parse()
        .expect("METRICS_ADDR must be a valid socket address");
    let concurrency: usize = std::env::var("MATCH_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        });
    // Stub mode: skip HMM entirely and echo back the raw coord as a successful match.
    // Use MATCH_STUB=1 to measure pure pipeline overhead without any compute cost.
    let stub = std::env::var("MATCH_STUB").is_ok();
    // Phase 1 streaming-match: when on, the matcher caches each vehicle's
    // last matched coord and feeds it as MatchOptions::anchor on the next
    // event, reducing the trellis from 6 layers to 2. 1-best (no multi-
    // hypothesis frontier preservation). Off by default — must be opted
    // into per pod via MATCH_STATEFUL=1.
    let stateful = std::env::var("MATCH_STATEFUL").is_ok();
    // Eviction TTL: vehicles whose `last_event_ms` is older than this on
    // the next event tick are dropped from the cache (we then cold-start
    // them). Aligned with HISTORY_MAX_AGE_SECS per the design doc.
    let state_ttl_ms: u64 = std::env::var("MATCH_STATE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1800)
        * 1000;
    // Phase 1B cum-cost divergence guard: if the running Viterbi
    // cumulative cost passes this ceiling, evict the warm state and
    // force the next event to cold-start. Prevents a bad warm-step
    // (e.g., GPS noise + ambiguous junction) from compounding cost
    // indefinitely. 0 = disabled.
    let cost_ceiling: u32 = std::env::var("MATCH_COST_CEILING")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000);

    tokio::spawn(metrics::serve_matcher(metrics_addr));

    let strategy = GeohashStrategy::with_precision(shard_precision);

    let shard: S = if let Ok(val) = std::env::var("OWNED_SHARD") {
        S::from_str(&val).expect("OWNED_SHARD is not a valid geohash")
    } else {
        #[cfg(debug_assertions)]
        { panic!("OWNED_SHARD must be set in debug builds") }

        #[cfg(not(debug_assertions))]
        {
            let nc = async_nats::connect(&nats_url)
                .await
                .map_err(|e| anyhow::anyhow!("NATS connect: {e}"))?;
            let js = async_nats::jetstream::new(nc);
            let assignment =
                routers_realtime::assignment::NatsKvAssignment::new(js, "shard-leases")
                    .await
                    .map_err(|e| anyhow::anyhow!("NatsKvAssignment: {e}"))?;
            assignment.acquire().await
        }
    };

    // OwnedAndPadded loads only the owned shard file. Cross-boundary nodes
    // (within 1km of the cell edge) are baked into that file at shard-build
    // time, so the runtime graph stays small but boundary edges remain
    // resolvable. The selection's loaded set is always {owned}, so we use
    // ShardedNetwork directly — wrapping it in MultiShardNetwork would
    // duplicate `hash`, `graph`, and both RTrees, roughly doubling RAM
    // for no functional benefit (ShardedNetwork already implements
    // DataPlane + Scan + Route + Discovery).
    let _selection = Selection::new(
        &strategy,
        shard,
        SelectionMode::OwnedAndPadded { padding_distance: 1000.0 },
    );
    let fetcher = FileFetcher::new(std::path::Path::new(&shard_dir));
    let mut loader = ShardLoader::<E, M, S, _, _>::new(fetcher, shard_filename);

    let network = loader.load(&shard).await?;
    let m = metrics::matcher_global();

    // Per-vehicle state cache for streaming-match. When `stateful` is
    // false this stays empty and is never read — zero overhead.
    let state_cache: StateCache = Arc::new(DashMap::with_capacity(if stateful { 16_384 } else { 0 }));
    log::info!("[matcher-{shard}] concurrency={concurrency} stub={stub} stateful={stateful}");

    // Background eviction + gauge updater. Scans the cache every 60s
    // and drops entries whose `last_event_ms` is older than the TTL.
    // No-op while `state_cache` is empty (stateful=false case).
    if stateful {
        let evict_cache = Arc::clone(&state_cache);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await; // skip first immediate tick
            loop {
                tick.tick().await;
                let cutoff = now_ms().saturating_sub(state_ttl_ms);
                evict_cache.retain(|_, s| s.last_event_ms >= cutoff);
                metrics::matcher_global()
                    .state_cache_size
                    .set(evict_cache.len() as f64);
            }
        });
    }

    let mut connect_backoff = Duration::from_secs(1);

    'reconnect: loop {
        // ── Connect with retry ───────────────────────────────────────────────
        let nc = loop {
            match async_nats::connect(&nats_url).await {
                Ok(nc) => {
                    connect_backoff = Duration::from_secs(1);
                    break nc;
                }
                Err(e) => {
                    eprintln!("[matcher-{shard}] NATS connect: {e}, retry in {connect_backoff:?}");
                    tokio::time::sleep(connect_backoff).await;
                    connect_backoff = (connect_backoff * 2).min(Duration::from_secs(30));
                }
            }
        };

        // js is still needed for the NatsKvAssignment path (shard auto-assignment without OWNED_SHARD)
        let js = async_nats::jetstream::new(nc.clone());
        let _ = js; // suppress unused warning when OWNED_SHARD is always set

        let mut sub = match nc.subscribe(format!("match.{}", shard)).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[matcher-{shard}] subscribe: {e}, reconnecting");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue 'reconnect;
            }
        };

        let route_nc = nc.clone();
        let result_sink = nats::result_sink(nc, "matched.positions".into());
        futures::pin_mut!(result_sink);
        let route_sink = nats::route_sink(route_nc, "matched.routes".into());
        futures::pin_mut!(route_sink);

        // ── Pipeline: up to `concurrency` parallel HMM solves ────────────────
        // select! simultaneously waits for a completed solve (to publish and
        // free a slot) and the next incoming message (to fill a free slot).
        // Backpressure: the messages arm is disabled when the join_set is full.
        let mut join_set: tokio::task::JoinSet<SolveResult> = tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                biased;

                // Drain a completed solve and publish its result.
                Some(task_result) = join_set.join_next(), if !join_set.is_empty() => {
                    let (vehicle_id, resolved_at_ms, current_coord, debug_pts, match_result, solve_ms, t_delivery, was_warm, t_spawn_at, prev_argmin_edge) =
                        match task_result {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("[matcher-{shard}] task panicked: {e}");
                                continue;
                            }
                        };

                    m.solve_latency_ms.observe(solve_ms);
                    let setup_ms = (t_spawn_at - t_delivery).as_secs_f64() * 1000.0;
                    let t_post_start = Instant::now();

                    match match_result {
                        Ok((path, _)) if path.discretized.elements.is_empty() => {
                            m.matches_no_candidate.inc();
                            let result = MatchResult {
                                vehicle_id: vehicle_id.clone(),
                                resolved_at_ms,
                                matched_at_ms: now_ms(),
                                coord: current_coord,
                                outcome: MatchOutcome::NoCandidate,
                            };
                            if let Err(e) = result_sink.send(result).await {
                                eprintln!("[matcher-{shard}] result publish: {e}, reconnecting");
                                continue 'reconnect;
                            }
                        }
                        Ok((path, mut new_state)) => {
                            m.matches_success.inc();

                            let snapped_coord = path
                                .discretized
                                .elements
                                .last()
                                .map(|el| Point::from(el.point));

                            if let Some(snapped) = snapped_coord {
                                if stateful {
                                    if let Some(k) = frontier_k() {
                                        new_state.truncate_to_top(k);
                                    }
                                    let new_cum_cost = new_state.last_cum_cost().unwrap_or(0);
                                    m.frontier_size.observe(new_state.len() as f64);
                                    if was_warm {
                                        if let (Some(prev_edge), Some(new_node)) =
                                            (prev_argmin_edge, new_state.argmin())
                                        {
                                            if new_node.edge.id != prev_edge {
                                                m.argmin_revisions.inc();
                                            }
                                        }
                                    }
                                    state_cache
                                        .entry(vehicle_id.clone())
                                        .and_modify(|s| {
                                            if resolved_at_ms > s.last_event_ms {
                                                *s = new_state.clone();
                                            }
                                        })
                                        .or_insert(new_state);
                                    m.cum_cost.observe(new_cum_cost as f64);
                                }

                                let result = MatchResult {
                                    vehicle_id: vehicle_id.clone(),
                                    resolved_at_ms,
                                    matched_at_ms: now_ms(),
                                    coord: snapped,
                                    outcome: MatchOutcome::Success,
                                };
                                if let Err(e) = result_sink.send(result).await {
                                    eprintln!("[matcher-{shard}] result publish: {e}, reconnecting");
                                    continue 'reconnect;
                                }
                            }

                            let route = MatchRoute {
                                vehicle_id: vehicle_id.clone(),
                                resolved_at_ms,
                                polyline: path
                                    .interpolated
                                    .elements
                                    .iter()
                                    .map(|el| el.point)
                                    .collect(),
                            };
                            if let Err(e) = route_sink.send(route).await {
                                eprintln!("[matcher-{shard}] route publish: {e}, reconnecting");
                                continue 'reconnect;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "match failed for vehicle {}: {e:?} | points={} warm={} linestring=[{}]",
                                vehicle_id,
                                debug_pts.len(),
                                was_warm,
                                debug_pts.join(",")
                            );
                            m.matches_error.inc();
                            // Drop stale state on error so the next event
                            // for this vehicle cold-starts with fresh history.
                            if stateful && was_warm {
                                state_cache.remove(&vehicle_id);
                            }
                        }
                    }

                    let total_ms = t_delivery.elapsed().as_secs_f64() * 1000.0;
                    m.match_latency_ms.observe(total_ms);

                    // Sampled per-event timeline breakdown. setup_ms is
                    // matcher-binary overhead (decode + sanity + state
                    // lookup + linestring build + tokio dispatch hop).
                    // solve_ms is the solver wall-clock. post_ms is the
                    // publish + state writeback path. total_ms is
                    // delivery → publish complete.
                    let sample_n = match_profile_sample_n();
                    if sample_n > 0 && MATCH_PROFILE_COUNTER.fetch_add(1, Ordering::Relaxed) % sample_n == 0 {
                        let post_ms = t_post_start.elapsed().as_secs_f64() * 1000.0;
                        log::info!(
                            target: "match_profile",
                            "total={:.2}ms setup={:.2}ms solve={:.2}ms post={:.2}ms warm={} vid={}",
                            total_ms, setup_ms, solve_ms, post_ms, was_warm, vehicle_id,
                        );
                    }
                }

                // Accept the next message when below capacity.
                // In stub mode the join_set is always empty so this arm is always enabled.
                msg_opt = sub.next(), if join_set.len() < concurrency => {
                    let msg = match msg_opt {
                        None => {
                            eprintln!("[matcher-{shard}] stream closed, reconnecting");
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            continue 'reconnect;
                        }
                        Some(m) => m,
                    };

                    let t_delivery = Instant::now();

                    let ctx: MatchContext<S> = match postcard::from_bytes(&msg.payload) {
                        Ok(c) => c,
                        Err(e) => {
                            log::warn!("failed to decode MatchContext: {e}");
                            continue;
                        }
                    };

                    if stub {
                        // Echo the raw coord back as an instant successful match.
                        // No HMM, no spawn_blocking — pure pipeline overhead measurement.
                        let result = MatchResult {
                            vehicle_id: ctx.vehicle_id,
                            resolved_at_ms: ctx.resolved_at_ms,
                            matched_at_ms: now_ms(),
                            coord: ctx.current.coord,
                            outcome: MatchOutcome::Success,
                        };
                        m.matches_success.inc();
                        if let Err(e) = result_sink.send(result).await {
                            eprintln!("[matcher-{shard}] stub publish: {e}, reconnecting");
                            continue 'reconnect;
                        }
                        m.match_latency_ms
                            .observe(t_delivery.elapsed().as_secs_f64() * 1000.0);
                        continue;
                    }

                    let points: Vec<Point> = ctx
                        .history
                        .iter()
                        .chain(std::iter::once(&ctx.current))
                        .map(|p| p.coord)
                        .collect();

                    // Sanity-guard pathological inputs before they hog a
                    // concurrency slot. A single GPS jump > 5km is almost
                    // always bad data (vehicle teleport, sensor reset).
                    // Total length > 50km on a window of ≤11 points implies
                    // we'd be matching across an absurd geographic span and
                    // the HMM cost will blow up — fail fast as a soft error.
                    const MAX_SEGMENT_METERS: f64 = 5_000.0;
                    const MAX_TOTAL_METERS: f64 = 50_000.0;
                    let (max_gap, total_len) = points
                        .windows(2)
                        .map(|w| Haversine.distance(w[0], w[1]))
                        .fold((0.0_f64, 0.0_f64), |(mx, sum), d| (mx.max(d), sum + d));
                    if max_gap > MAX_SEGMENT_METERS || total_len > MAX_TOTAL_METERS {
                        log::warn!(
                            "skipping pathological trip vehicle={} max_gap={:.0}m total={:.0}m points={}",
                            ctx.vehicle_id,
                            max_gap,
                            total_len,
                            points.len(),
                        );
                        m.matches_error.inc();
                        let result = MatchResult {
                            vehicle_id: ctx.vehicle_id,
                            resolved_at_ms: ctx.resolved_at_ms,
                            matched_at_ms: now_ms(),
                            coord: ctx.current.coord,
                            outcome: MatchOutcome::Error,
                        };
                        if let Err(e) = result_sink.send(result).await {
                            eprintln!("[matcher-{shard}] error publish: {e}, reconnecting");
                            continue 'reconnect;
                        }
                        m.match_latency_ms
                            .observe(t_delivery.elapsed().as_secs_f64() * 1000.0);
                        continue;
                    }

                    let debug_pts: Vec<String> = ctx
                        .history
                        .iter()
                        .chain(std::iter::once(&ctx.current))
                        .map(|p| {
                            format!(
                                "[{:.6},{:.6},t={}]",
                                p.coord.x(),
                                p.coord.y(),
                                p.timestamp_ms
                            )
                        })
                        .collect();

                    // Decide warm vs cold: if we have a non-stale frontier
                    // for this vehicle that's still under the cost ceiling,
                    // extend it by one observation via `StreamingMatcher::step`.
                    // Otherwise cold-start with the full history linestring,
                    // recording the reason for downstream observability.
                    let now_ms_local = now_ms();
                    let (warm_state, cold_reason): (Option<MatchState<E>>, Option<&'static str>) =
                        if stateful {
                            let read = state_cache.get(&ctx.vehicle_id);
                            match read {
                                Some(s) => {
                                    let cum = s.last_cum_cost().unwrap_or(u32::MAX);
                                    let ttl_ok = now_ms_local
                                        .saturating_sub(s.last_event_ms)
                                        <= state_ttl_ms;
                                    let event_ahead = ctx.resolved_at_ms > s.last_event_ms;
                                    let under_ceiling = cum < cost_ceiling;
                                    if !under_ceiling {
                                        drop(s);
                                        state_cache.remove(&ctx.vehicle_id);
                                        m.cost_ceiling_evictions.inc();
                                        (None, Some("cost_ceiling"))
                                    } else if !ttl_ok {
                                        (None, Some("ttl_expired"))
                                    } else if !event_ahead {
                                        (None, Some("stale_event"))
                                    } else if s.is_empty() {
                                        (None, Some("empty_frontier"))
                                    } else {
                                        (Some(s.clone()), None)
                                    }
                                }
                                None => (None, Some("no_state")),
                            }
                        } else {
                            (None, None)
                        };

                    let was_warm = warm_state.is_some();
                    if was_warm {
                        m.match_step_warm.inc();
                    } else {
                        m.match_step_cold.inc();
                        if let Some(reason) = cold_reason {
                            m.cold_start_reason.with_label_values(&[reason]).inc();
                        }
                    }

                    // Snapshot prev argmin edge before moving warm_state into the closure.
                    // Drives the argmin_revisions metric in the drain.
                    let prev_argmin_edge: Option<DirectionAwareEdgeId<E>> = warm_state
                        .as_ref()
                        .and_then(|s| s.argmin())
                        .map(|n| n.edge.id);

                    let cold_points: Vec<Point> = if was_warm {
                        Vec::new()
                    } else {
                        points
                    };
                    let new_point = ctx.current.coord;

                    let vehicle_id = ctx.vehicle_id.clone();
                    let resolved_at_ms = ctx.resolved_at_ms;
                    let current_coord = ctx.current.coord;
                    let network_clone = Arc::clone(&network);

                    // Captured *before* spawn_blocking dispatches the
                    // task. Setup time = t_spawn_at - t_delivery, used
                    // to separate matcher-binary overhead from solver work.
                    let t_spawn_at = Instant::now();

                    join_set.spawn_blocking(move || {
                        let costing = CostingStrategies::<_, _, E, M, _>::default();
                        let matcher = StreamingMatcher::new(
                            &*network_clone,
                            &costing,
                            DEFAULT_SEARCH_DISTANCE,
                        );
                        let runtime = OsmEdgeMetadata::runtime(None);

                        let t_solve = Instant::now();
                        let result = match warm_state {
                            Some(prev) => matcher.step(&prev, new_point, resolved_at_ms, &runtime),
                            None => {
                                let linestring = LineString(
                                    cold_points.iter().map(|p| (*p).into()).collect(),
                                );
                                matcher.cold_start(linestring, resolved_at_ms, &runtime)
                            }
                        };
                        let solve_ms = t_solve.elapsed().as_secs_f64() * 1000.0;
                        if solve_ms > 200.0 {
                            log::warn!(
                                "slow solve {:.0}ms vehicle={} points={} warm={}",
                                solve_ms,
                                vehicle_id,
                                debug_pts.len(),
                                was_warm,
                            );
                        }
                        (vehicle_id, resolved_at_ms, current_coord, debug_pts, result, solve_ms, t_delivery, was_warm, t_spawn_at, prev_argmin_edge)
                    });
                }
            }
        }
    }
}
