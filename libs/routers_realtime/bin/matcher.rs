use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use geo::{Distance, Haversine, LineString, Point};
use routers::transition::r#match::{Anchor, MatchOptions};
use routers::transition::streaming::MatchState;
use routers::transition::{MatchError, RoutedPath};
use routers::Match;
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

/// State cache type: vehicle_id → MatchState. Anchored 1-best warm step.
type StateCache = Arc<DashMap<String, MatchState>>;

type E = OsmEntryId;
type M = OsmEdgeMetadata;
type S = Geohash;
type SolveResult = (
    String,
    u64,
    geo::Point,
    Vec<String>,
    Result<RoutedPath<E, M>, MatchError>,
    f64,
    Instant,
    bool,    // was_warm — true if anchor-based warm step was used
    Instant, // t_spawn_at
    u32,     // prev_cum_cost — Viterbi cumulative cost prior to this event (0 on cold-start)
);

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
                    let (vehicle_id, resolved_at_ms, current_coord, debug_pts, match_result, solve_ms, t_delivery, was_warm, t_spawn_at, prev_cum_cost) =
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
                        Ok(path) if path.discretized.elements.is_empty() => {
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
                        Ok(path) => {
                            m.matches_success.inc();

                            let snapped_coord = path
                                .discretized
                                .elements
                                .last()
                                .map(|el| Point::from(el.point));

                            if let Some(snapped) = snapped_coord {
                                // Phase 1B cum_cost tracking:
                                //   warm → prev_cum_cost + this_event_cost
                                //   cold → reset to this_event_cost (no prior chain)
                                let event_cost = path.cost;
                                let new_cum_cost = if was_warm {
                                    prev_cum_cost.saturating_add(event_cost)
                                } else {
                                    event_cost
                                };
                                // Write back to state cache (stateful mode
                                // only — DashMap insert is O(1) amortised).
                                // Last-writer-by-timestamp: only commit if
                                // our resolved_at_ms is the newest seen.
                                if stateful {
                                    state_cache
                                        .entry(vehicle_id.clone())
                                        .and_modify(|s| {
                                            if resolved_at_ms > s.last_event_ms {
                                                s.last_matched = snapped;
                                                s.last_event_ms = resolved_at_ms;
                                                s.last_cum_cost = new_cum_cost;
                                            }
                                        })
                                        .or_insert_with(|| MatchState::new(snapped, resolved_at_ms, new_cum_cost));
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

                    // Decide warm vs cold: if we have non-stale state for
                    // this vehicle, build a 2-point linestring `[current]`
                    // with anchor=prev.last_matched; otherwise cold-start
                    // with the full 6-point history-based linestring.
                    //
                    // Three gates for going warm:
                    //   1. State cache hit, AND
                    //   2. Not TTL-expired, AND
                    //   3. cum_cost hasn't passed the ceiling.
                    // The cost-ceiling check catches drift: if the warm
                    // step's path quality has degraded over many events,
                    // we forcibly re-anchor by going cold.
                    let now_ms_local = now_ms();
                    let (warm_anchor, prev_cum_cost): (Option<Anchor>, u32) = if stateful {
                        let read = state_cache.get(&ctx.vehicle_id);
                        match read {
                            Some(s)
                                if now_ms_local.saturating_sub(s.last_event_ms) <= state_ttl_ms
                                    && ctx.resolved_at_ms > s.last_event_ms
                                    && s.last_cum_cost < cost_ceiling =>
                            {
                                (
                                    Some(Anchor { coord: s.last_matched.into() }),
                                    s.last_cum_cost,
                                )
                            }
                            Some(s) if s.last_cum_cost >= cost_ceiling => {
                                // Cost-ceiling trip-wire fired: drop state
                                // and cold-start this event to re-anchor.
                                drop(s);
                                state_cache.remove(&ctx.vehicle_id);
                                m.cost_ceiling_evictions.inc();
                                (None, 0)
                            }
                            _ => (None, 0),
                        }
                    } else {
                        (None, 0)
                    };

                    let (linestring, was_warm) = if let Some(anchor) = warm_anchor {
                        // Warm step: only the current GPS point in the
                        // linestring — `r#match` will prepend the anchor.
                        m.match_step_warm.inc();
                        let ls = LineString(vec![ctx.current.coord.into()]);
                        // Stash anchor for the closure below.
                        let _ = anchor; // anchor consumed via MatchOptions below
                        (ls, true)
                    } else {
                        m.match_step_cold.inc();
                        (LineString(points.iter().map(|p| (*p).into()).collect()), false)
                    };

                    let vehicle_id = ctx.vehicle_id.clone();
                    let resolved_at_ms = ctx.resolved_at_ms;
                    let current_coord = ctx.current.coord;
                    let network_clone = Arc::clone(&network);
                    let anchor_for_solve = warm_anchor;
                    let prev_cum_cost_capture = prev_cum_cost;

                    // Captured *before* spawn_blocking dispatches the
                    // task. Setup time = t_spawn_at - t_delivery. We
                    // need this on the result side to separate
                    // matcher-binary overhead (decode, sanity, state
                    // lookup, linestring build, dispatch hop) from the
                    // actual solver work.
                    let t_spawn_at = Instant::now();

                    join_set.spawn_blocking(move || {
                        let opts = MatchOptions::<E, M, _>::default()
                            .with_anchor(anchor_for_solve);
                        let t_solve = Instant::now();
                        let result = network_clone.r#match(linestring, opts);
                        let solve_ms = t_solve.elapsed().as_secs_f64() * 1000.0;
                        // Surface slow solves so we can correlate them
                        // with vehicle traces if the p99 starts climbing again.
                        if solve_ms > 200.0 {
                            log::warn!(
                                "slow solve {:.0}ms vehicle={} points={} warm={}",
                                solve_ms,
                                vehicle_id,
                                debug_pts.len(),
                                was_warm,
                            );
                        }
                        (vehicle_id, resolved_at_ms, current_coord, debug_pts, result, solve_ms, t_delivery, was_warm, t_spawn_at, prev_cum_cost_capture)
                    });
                }
            }
        }
    }
}
