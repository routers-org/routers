use futures::SinkExt;
use futures::StreamExt;
use geo::{LineString, Point};
use routers::transition::r#match::MatchOptions;
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
    FileFetcher, Geohash, GeohashStrategy, MultiShardNetwork, Selection, SelectionMode,
    ShardLoader, ShardingStrategy,
};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type E = OsmEntryId;
type M = OsmEdgeMetadata;
type S = Geohash;

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
        .unwrap_or(5);
    let metrics_addr: std::net::SocketAddr = std::env::var("METRICS_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9092".into())
        .parse()
        .expect("METRICS_ADDR must be a valid socket address");

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

    let selection = Selection::new(&strategy, shard, SelectionMode::OwnedAndNeighbours);
    let fetcher = FileFetcher::new(std::path::Path::new(&shard_dir));
    let mut loader = ShardLoader::<E, M, S, _, _>::new(fetcher, shard_filename);

    let owned_arc = loader.load(&shard).await?;
    let mut shards = vec![owned_arc];

    for neighbour in strategy.neighbours(&shard) {
        if selection.contains(&neighbour) {
            match loader.load(&neighbour).await {
                Ok(net) => shards.push(net),
                Err(e) => {
                    log::warn!("neighbour shard {} unavailable: {}", neighbour, e);
                }
            }
        }
    }

    let network = MultiShardNetwork::<E, M, S>::new(shards);
    let m = metrics::matcher_global();

    // Tracks previously emitted (vehicle_id, resolved_at_ms) → coord so we can detect
    // when the HMM revises a historical position and emit it to matched.corrections.
    let mut last_results: std::collections::HashMap<(String, u64), geo::Coord> = std::collections::HashMap::new();

    // Reconnect loop — recovers from both startup connection failures and
    // mid-run transient errors (missed idle heartbeat, stream resets, etc.).
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

        let js = async_nats::jetstream::new(nc.clone());

        let consumer = match nats::match_consumer(&js, &shard).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[matcher-{shard}] consumer setup: {e}, reconnecting");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue 'reconnect;
            }
        };

        let route_nc = nc.clone();
        let corrections_nc = nc.clone();
        let result_sink = nats::result_sink(nc, "matched.positions".into());
        futures::pin_mut!(result_sink);
        let route_sink = nats::route_sink(route_nc, "matched.routes".into());
        futures::pin_mut!(route_sink);
        let corrections_sink = nats::result_sink(corrections_nc, "matched.corrections".into());
        futures::pin_mut!(corrections_sink);

        let mut messages = match consumer.messages().await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[matcher-{shard}] message stream: {e}, reconnecting");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue 'reconnect;
            }
        };

        // ── Message loop ─────────────────────────────────────────────────────
        loop {
            let msg = match messages.next().await {
                None => {
                    eprintln!("[matcher-{shard}] stream closed, reconnecting");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue 'reconnect;
                }
                Some(Ok(msg)) => msg,
                Some(Err(e)) => {
                    eprintln!("[matcher-{shard}] message recv: {e}, reconnecting");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue 'reconnect;
                }
            };

            let t_delivery = Instant::now();

            let ctx: MatchContext<S> = match postcard::from_bytes(&msg.payload) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("failed to decode MatchContext: {e}");
                    let _ = msg.ack().await;
                    continue;
                }
            };

            let coords: Vec<geo::Coord> = ctx
                .history
                .iter()
                .chain(std::iter::once(&ctx.current))
                .map(|p| p.coord.into())
                .collect();

            let linestring = LineString(coords);
            let debug_pts: Vec<String> = ctx.history.iter()
                .chain(std::iter::once(&ctx.current))
                .map(|p| format!("[{:.6},{:.6},t={}]", p.coord.x(), p.coord.y(), p.timestamp_ms))
                .collect();
            let opts = MatchOptions::<E, M, _>::default();

            let t_solve = Instant::now();
            let match_result = network.r#match(linestring, opts);
            let solve_ms = t_solve.elapsed().as_secs_f64() * 1000.0;
            m.solve_latency_ms.observe(solve_ms);

            match match_result {
                Ok(path) if path.discretized.elements.is_empty() => {
                    // HMM returned no candidates — emit one NoCandidate result for current
                    m.matches_no_candidate.inc();
                    let result = MatchResult {
                        vehicle_id: ctx.vehicle_id,
                        resolved_at_ms: ctx.resolved_at_ms,
                        matched_at_ms: now_ms(),
                        coord: ctx.current.coord,
                        outcome: MatchOutcome::NoCandidate,
                    };
                    if let Err(e) = result_sink.send(result).await {
                        eprintln!("[matcher-{shard}] result publish: {e}, reconnecting");
                        let _ = msg.ack().await;
                        continue 'reconnect;
                    }
                }
                Ok(path) => {
                    m.matches_success.inc();

                    // Emit one MatchResult per discretized element (full-window emission).
                    // Also detect corrections: same (vehicle_id, resolved_at_ms) key with
                    // a meaningfully different coord (> ~1m) indicates the HMM revised its
                    // earlier decision as more context arrived.
                    let history_len = ctx.history.len();
                    let matched_at = now_ms();
                    for (i, el) in path.discretized.elements.iter().enumerate() {
                        let resolved_at_ms = if i < history_len {
                            ctx.history[i].resolved_at_ms
                        } else {
                            ctx.resolved_at_ms
                        };
                        let new_coord = geo::Coord::from(el.point);
                        let key = (ctx.vehicle_id.clone(), resolved_at_ms);

                        let result = MatchResult {
                            vehicle_id: ctx.vehicle_id.clone(),
                            resolved_at_ms,
                            matched_at_ms: matched_at,
                            coord: Point::from(el.point),
                            outcome: MatchOutcome::Success,
                        };

                        // Emit correction if this key was previously published with a different coord.
                        // Threshold: ~1m (1e-5 deg ≈ 1.1m; using squared Euclidean as a fast proxy).
                        if let Some(&prev) = last_results.get(&key) {
                            let dx = prev.x - new_coord.x;
                            let dy = prev.y - new_coord.y;
                            if dx * dx + dy * dy > 1e-10 {
                                if let Err(e) = corrections_sink.send(result.clone()).await {
                                    eprintln!("[matcher-{shard}] correction publish: {e}, reconnecting");
                                    let _ = msg.ack().await;
                                    continue 'reconnect;
                                }
                            }
                        }

                        if let Err(e) = result_sink.send(result).await {
                            eprintln!("[matcher-{shard}] result publish: {e}, reconnecting");
                            let _ = msg.ack().await;
                            continue 'reconnect;
                        }

                        last_results.insert(key, new_coord);
                    }

                    // Prune last_results entries older than the history window (5 min + buffer).
                    // This keeps memory bounded at ~max_history_points entries per active vehicle.
                    let cutoff = ctx.resolved_at_ms.saturating_sub(600_000);
                    last_results.retain(|(_vid, resolved_at_ms), _| *resolved_at_ms >= cutoff);

                    // Emit one MatchRoute with the full interpolated road geometry
                    let route = MatchRoute {
                        vehicle_id: ctx.vehicle_id.clone(),
                        resolved_at_ms: ctx.resolved_at_ms,
                        polyline: path.interpolated.elements.iter().map(|el| el.point).collect(),
                    };
                    if let Err(e) = route_sink.send(route).await {
                        eprintln!("[matcher-{shard}] route publish: {e}, reconnecting");
                        let _ = msg.ack().await;
                        continue 'reconnect;
                    }
                }
                Err(e) => {
                    eprintln!(
                        "match failed for vehicle {}: {e:?} | points={} linestring=[{}]",
                        ctx.vehicle_id,
                        debug_pts.len(),
                        debug_pts.join(",")
                    );
                    m.matches_error.inc();
                    let _ = msg.ack().await;
                    continue;
                }
            }

            let _ = msg.ack().await;

            m.match_latency_ms
                .observe(t_delivery.elapsed().as_secs_f64() * 1000.0);
        }
    }
}
