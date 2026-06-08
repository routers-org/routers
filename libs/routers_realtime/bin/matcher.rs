use futures::SinkExt;
use futures::StreamExt;
use geo::{LineString, Point};
use routers::transition::r#match::MatchOptions;
use routers::Match;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_realtime::{
    context::{MatchContext, MatchOutcome, MatchResult},
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
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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

    // OWNED_SHARD env var always takes precedence (required in debug builds; optional override
    // in release builds while NatsKvAssignment lease acquisition is not yet implemented).
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

    let nc = async_nats::connect(&nats_url)
        .await
        .map_err(|e| anyhow::anyhow!("NATS connect: {e}"))?;
    let js = async_nats::jetstream::new(nc.clone());
    let consumer = nats::match_consumer(&js, &shard)
        .await
        .map_err(|e| anyhow::anyhow!("match_consumer: {e}"))?;
    let result_sink = nats::result_sink(nc, "matched.positions".into());

    futures::pin_mut!(result_sink);
    let mut messages = consumer
        .messages()
        .await
        .map_err(|e| anyhow::anyhow!("consumer messages: {e}"))?;

    let m = metrics::matcher_global();

    while let Some(msg) = messages.next().await {
        let t_delivery = Instant::now();

        let msg = msg.map_err(|e| anyhow::anyhow!("message recv: {e}"))?;
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
        let (matched_coord, outcome) = match network.r#match(linestring, opts) {
            Ok(path) => {
                let coord = path
                    .discretized
                    .elements
                    .last()
                    .map(|el| Point::from(el.point))
                    .unwrap_or(ctx.current.coord);
                let outcome = if path.discretized.elements.is_empty() {
                    MatchOutcome::NoCandidate
                } else {
                    MatchOutcome::Success
                };
                (coord, outcome)
            }
            Err(e) => {
                eprintln!(
                    "match failed for vehicle {}: {e:?} | points={} linestring=[{}]",
                    ctx.vehicle_id,
                    debug_pts.len(),
                    debug_pts.join(",")
                );
                (ctx.current.coord, MatchOutcome::Error)
            }
        };
        let solve_ms = t_solve.elapsed().as_secs_f64() * 1000.0;
        m.solve_latency_ms.observe(solve_ms);

        match outcome {
            MatchOutcome::Success => m.matches_success.inc(),
            MatchOutcome::NoCandidate => m.matches_no_candidate.inc(),
            MatchOutcome::Error => {
                m.matches_error.inc();
                let _ = msg.ack().await;
                continue;
            }
        }

        let matched_at_ms = now_ms();
        let result = MatchResult {
            vehicle_id: ctx.vehicle_id,
            resolved_at_ms: ctx.resolved_at_ms,
            matched_at_ms,
            coord: matched_coord,
            outcome,
        };

        result_sink.send(result).await?;
        let _ = msg.ack().await;

        let match_ms = t_delivery.elapsed().as_secs_f64() * 1000.0;
        m.match_latency_ms.observe(match_ms);
    }

    Ok(())
}
