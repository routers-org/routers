use futures::{SinkExt, StreamExt};
use geo::{LineString, Point};
use routers::transition::r#match::MatchOptions;
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
    FileFetcher, Geohash, GeohashStrategy, MultiShardNetwork, Selection, SelectionMode,
    ShardLoader, ShardingStrategy,
};
use std::sync::Arc;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

type E = OsmEntryId;
type M = OsmEdgeMetadata;
type S = Geohash;
type SolveResult = (
    async_nats::jetstream::Message,
    String,
    u64,
    geo::Point,
    Vec<String>,
    Result<RoutedPath<E, M>, MatchError>,
    f64,
    Instant,
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
        .unwrap_or(5);
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

    let network = Arc::new(MultiShardNetwork::<E, M, S>::new(shards));
    let m = metrics::matcher_global();

    log::info!("[matcher-{shard}] concurrency={concurrency} stub={stub}");

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
        let result_sink = nats::result_sink(nc, "matched.positions".into());
        futures::pin_mut!(result_sink);
        let route_sink = nats::route_sink(route_nc, "matched.routes".into());
        futures::pin_mut!(route_sink);

        let mut messages = match consumer.messages().await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[matcher-{shard}] message stream: {e}, reconnecting");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue 'reconnect;
            }
        };

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
                    let (msg, vehicle_id, resolved_at_ms, current_coord, debug_pts, match_result, solve_ms, t_delivery) =
                        match task_result {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("[matcher-{shard}] task panicked: {e}");
                                continue;
                            }
                        };

                    m.solve_latency_ms.observe(solve_ms);

                    match match_result {
                        Ok(path) if path.discretized.elements.is_empty() => {
                            m.matches_no_candidate.inc();
                            let result = MatchResult {
                                vehicle_id,
                                resolved_at_ms,
                                matched_at_ms: now_ms(),
                                coord: current_coord,
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

                            if let Some(last_el) = path.discretized.elements.last() {
                                let result = MatchResult {
                                    vehicle_id: vehicle_id.clone(),
                                    resolved_at_ms,
                                    matched_at_ms: now_ms(),
                                    coord: Point::from(last_el.point),
                                    outcome: MatchOutcome::Success,
                                };
                                if let Err(e) = result_sink.send(result).await {
                                    eprintln!("[matcher-{shard}] result publish: {e}, reconnecting");
                                    let _ = msg.ack().await;
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
                                let _ = msg.ack().await;
                                continue 'reconnect;
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "match failed for vehicle {}: {e:?} | points={} linestring=[{}]",
                                vehicle_id,
                                debug_pts.len(),
                                debug_pts.join(",")
                            );
                            m.matches_error.inc();
                        }
                    }

                    let _ = msg.ack().await;
                    m.match_latency_ms
                        .observe(t_delivery.elapsed().as_secs_f64() * 1000.0);
                }

                // Accept the next message when below capacity.
                // In stub mode the join_set is always empty so this arm is always enabled.
                msg_result = messages.next(), if join_set.len() < concurrency => {
                    let msg = match msg_result {
                        None => {
                            eprintln!("[matcher-{shard}] stream closed, reconnecting");
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            continue 'reconnect;
                        }
                        Some(Ok(m)) => m,
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
                            let _ = msg.ack().await;
                            continue 'reconnect;
                        }
                        let _ = msg.ack().await;
                        m.match_latency_ms
                            .observe(t_delivery.elapsed().as_secs_f64() * 1000.0);
                        continue;
                    }

                    let coords: Vec<geo::Coord> = ctx
                        .history
                        .iter()
                        .chain(std::iter::once(&ctx.current))
                        .map(|p| p.coord.into())
                        .collect();
                    let linestring = LineString(coords);
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

                    let vehicle_id = ctx.vehicle_id.clone();
                    let resolved_at_ms = ctx.resolved_at_ms;
                    let current_coord = ctx.current.coord;
                    let network_clone = Arc::clone(&network);

                    join_set.spawn_blocking(move || {
                        let opts = MatchOptions::<E, M, _>::default();
                        let t_solve = Instant::now();
                        let result = network_clone.r#match(linestring, opts);
                        let solve_ms = t_solve.elapsed().as_secs_f64() * 1000.0;
                        (msg, vehicle_id, resolved_at_ms, current_coord, debug_pts, result, solve_ms, t_delivery)
                    });
                }
            }
        }
    }
}
