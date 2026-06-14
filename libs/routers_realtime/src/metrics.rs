use axum::{Router, routing::get};
use prometheus::{
    Gauge, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts, Registry,
    TextEncoder, opts,
};
use std::net::SocketAddr;
use std::sync::OnceLock;

// ── Matcher metrics ───────────────────────────────────────────────────────────

pub struct MatcherMetrics {
    /// Wall-clock time from NATS delivery to match completion (ms),
    /// labelled by `kind={warm,cold}` so the warm-step tail can be read
    /// directly without the cold-start mix dominating p99.
    pub match_latency_ms: HistogramVec,
    /// HMM solver wall time (ms), labelled by `kind={warm,cold}`.
    pub solve_latency_ms: HistogramVec,
    /// Matcher-binary setup overhead per event (ms): decode + sanity +
    /// state lookup + linestring build, ending at `spawn_blocking` dispatch.
    pub setup_ms: Histogram,
    /// Time the spawned closure spent waiting on a tokio blocking-thread
    /// slot before its first instruction ran (ms). Non-zero values mean
    /// `MATCH_CONCURRENCY` saturation, not solver slowness.
    pub queue_wait_ms: Histogram,
    /// Publish + state writeback path after the solve completes (ms).
    pub post_ms: Histogram,
    pub matches_success: IntCounter,
    pub matches_no_candidate: IntCounter,
    pub matches_error: IntCounter,
    // Phase 1 streaming-match counters.
    pub match_step_warm: IntCounter,
    pub match_step_cold: IntCounter,
    pub state_cache_size: Gauge,
    /// Cumulative Viterbi cost per warm step. Histogram lets us see the
    /// shape over time — if cum_cost tails out, the warm-step quality
    /// has degraded and we may need to reduce TTL or tighten the
    /// cost-ceiling.
    pub cum_cost: Histogram,
    /// Number of state evictions caused by the cum_cost ceiling guard
    /// (`MATCH_COST_CEILING`). Should be near-zero in steady state.
    pub cost_ceiling_evictions: IntCounter,
    /// Distribution of saved-frontier sizes after a warm step. With
    /// `MATCH_FRONTIER_K` set this caps at K; otherwise it traces the
    /// raw multi-candidate column produced by the solver.
    pub frontier_size: Histogram,
    /// Warm steps where the argmin candidate moved to a different
    /// edge between events. Elevated rates indicate volatile / noisy
    /// matching (GPS jitter at junctions, undersampled trips).
    pub argmin_revisions: IntCounter,
    /// Labelled counter of cold-start causes. Labels:
    ///   `no_state` — no cache entry for vehicle
    ///   `ttl_expired` — state present but `last_event_ms` past TTL
    ///   `stale_event` — incoming event older than cached state
    ///   `cost_ceiling` — cum_cost passed `MATCH_COST_CEILING`
    ///   `empty_frontier` — saved state has no hypotheses
    pub cold_start_reason: IntCounterVec,
    registry: Registry,
}

static MATCHER_METRICS: OnceLock<MatcherMetrics> = OnceLock::new();

pub fn matcher_global() -> &'static MatcherMetrics {
    MATCHER_METRICS.get_or_init(|| {
        let registry = Registry::new();

        macro_rules! counter {
            ($name:expr, $help:expr) => {{
                let c = IntCounter::with_opts(opts!($name, $help)).unwrap();
                registry.register(Box::new(c.clone())).unwrap();
                c
            }};
        }
        macro_rules! histogram {
            ($name:expr, $help:expr, $buckets:expr) => {{
                let h = Histogram::with_opts(
                    HistogramOpts::new($name, $help).buckets($buckets),
                )
                .unwrap();
                registry.register(Box::new(h.clone())).unwrap();
                h
            }};
        }

        // Latency buckets: 1ms → 10s, covering fast in-memory matches up to slow cold starts.
        let lat = vec![1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0, 10000.0];
        // Fine sub-ms buckets for the per-event pipeline stages — these
        // are expected to be small but spike when contended.
        let stage = vec![
            0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 1000.0,
        ];

        macro_rules! histogram_vec {
            ($name:expr, $help:expr, $buckets:expr, $labels:expr) => {{
                let h = HistogramVec::new(
                    HistogramOpts::new($name, $help).buckets($buckets),
                    $labels,
                )
                .unwrap();
                registry.register(Box::new(h.clone())).unwrap();
                h
            }};
        }

        MatcherMetrics {
            match_latency_ms: histogram_vec!(
                "routers_match_latency_ms",
                "End-to-end match latency: NATS delivery → result published (ms). \
                 reason=warm for warm steps, otherwise the cold-start cause.",
                lat.clone(),
                &["kind", "reason"]
            ),
            solve_latency_ms: histogram_vec!(
                "routers_solve_latency_ms",
                "HMM solver wall time (ms). reason=warm for warm steps, otherwise the cold-start cause.",
                lat.clone(),
                &["kind", "reason"]
            ),
            setup_ms: histogram!(
                "routers_match_setup_ms",
                "Matcher-binary setup overhead per event: decode + sanity + state lookup + dispatch (ms)",
                stage.clone()
            ),
            queue_wait_ms: histogram!(
                "routers_match_queue_wait_ms",
                "Time spent waiting for a blocking-thread slot before the solve closure started (ms)",
                stage.clone()
            ),
            post_ms: histogram!(
                "routers_match_post_ms",
                "Per-event publish + state writeback path after the solve completes (ms)",
                stage.clone()
            ),
            matches_success: counter!(
                "routers_matches_total_success",
                "Matches that produced a snapped coordinate"
            ),
            matches_no_candidate: counter!(
                "routers_matches_total_no_candidate",
                "Matches where HMM found no road candidates; raw GPS returned"
            ),
            matches_error: counter!(
                "routers_matches_total_error",
                "Matches that returned an algorithm error"
            ),
            match_step_warm: counter!(
                "routers_match_step_warm_total",
                "Warm steps: per-vehicle state was found and used as anchor"
            ),
            match_step_cold: counter!(
                "routers_match_step_cold_total",
                "Cold starts: no per-vehicle state, full history-based solve"
            ),
            state_cache_size: {
                let g = Gauge::with_opts(opts!(
                    "routers_match_state_cache_size",
                    "Number of vehicles currently in the streaming state cache"
                ))
                .unwrap();
                registry.register(Box::new(g.clone())).unwrap();
                g
            },
            cum_cost: {
                // Wide log-ish buckets: cum_cost can grow large over many events.
                let buckets = vec![
                    100.0, 500.0, 2_000.0, 10_000.0, 50_000.0,
                    200_000.0, 500_000.0, 1_000_000.0, 2_000_000.0, 5_000_000.0,
                ];
                let h = Histogram::with_opts(
                    HistogramOpts::new(
                        "routers_match_cum_cost",
                        "Cumulative Viterbi cost per warm-step (per event)",
                    )
                    .buckets(buckets),
                )
                .unwrap();
                registry.register(Box::new(h.clone())).unwrap();
                h
            },
            cost_ceiling_evictions: counter!(
                "routers_match_cost_ceiling_evictions_total",
                "State evictions triggered by the cum_cost ceiling guard"
            ),
            frontier_size: histogram!(
                "routers_match_frontier_size",
                "Number of hypotheses retained in the saved Viterbi frontier after a warm step",
                vec![1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0]
            ),
            argmin_revisions: counter!(
                "routers_match_argmin_revision_total",
                "Warm steps where the argmin candidate moved to a different edge"
            ),
            cold_start_reason: {
                let cv = IntCounterVec::new(
                    Opts::new(
                        "routers_match_cold_start_reason",
                        "Cold-start causes labelled by reason",
                    ),
                    &["reason"],
                )
                .unwrap();
                registry.register(Box::new(cv.clone())).unwrap();
                cv
            },
            registry,
        }
    })
}

pub async fn serve_matcher(addr: SocketAddr) {
    let app = Router::new().route("/metrics", get(render_matcher));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    log::info!("matcher metrics listening on http://{addr}/metrics");
    axum::serve(listener, app).await.unwrap();
}

async fn render_matcher() -> String {
    let encoder = TextEncoder::new();
    let families = matcher_global().registry.gather();
    encoder.encode_to_string(&families).unwrap_or_default()
}

pub struct Metrics {
    pub events_received: IntCounter,
    pub events_published: IntCounter,
    pub store_errors: IntCounter,
    /// End-to-end per-event latency: from AMQP receive to NATS ack (ms).
    pub total_latency_ms: Histogram,
    /// Time spent in the Valkey XADD+XREVRANGE pipeline (ms).
    pub store_latency_ms: Histogram,
    /// Time spent waiting for the NATS JetStream PubAck (ms).
    pub nats_latency_ms: Histogram,
    pub event_age_ms: Histogram,
    /// Exact age (seconds) of the most recently processed event.
    /// Unlike `event_age_ms`, this is a Gauge — no bucket saturation,
    /// works correctly for replay data that is days or months old.
    pub event_age_latest_s: Gauge,
    registry: Registry,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

pub fn global() -> &'static Metrics {
    METRICS.get_or_init(|| {
        let registry = Registry::new();

        macro_rules! counter {
            ($name:expr, $help:expr) => {{
                let c = IntCounter::with_opts(opts!($name, $help)).unwrap();
                registry.register(Box::new(c.clone())).unwrap();
                c
            }};
        }
        macro_rules! histogram {
            ($name:expr, $help:expr, $buckets:expr) => {{
                let h = Histogram::with_opts(
                    HistogramOpts::new($name, $help).buckets($buckets),
                )
                .unwrap();
                registry.register(Box::new(h.clone())).unwrap();
                h
            }};
        }
        macro_rules! gauge {
            ($name:expr, $help:expr) => {{
                let g = Gauge::with_opts(opts!($name, $help)).unwrap();
                registry.register(Box::new(g.clone())).unwrap();
                g
            }};
        }

        let fine = vec![0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0];

        Metrics {
            events_received: counter!(
                "routers_events_received_total",
                "AMQP events consumed"
            ),
            events_published: counter!(
                "routers_events_published_total",
                "Events published to NATS"
            ),
            store_errors: counter!(
                "routers_store_errors_total",
                "Position store errors"
            ),
            total_latency_ms: histogram!(
                "routers_total_latency_ms",
                "End-to-end per-event latency: AMQP receive → NATS ack (ms)",
                fine.clone()
            ),
            store_latency_ms: histogram!(
                "routers_store_latency_ms",
                "Valkey XADD+XREVRANGE pipeline latency (ms)",
                fine.clone()
            ),
            nats_latency_ms: histogram!(
                "routers_nats_latency_ms",
                "NATS JetStream publish + PubAck latency (ms)",
                fine.clone()
            ),
            event_age_ms: histogram!(
                "routers_event_age_ms",
                "Age of event when received: wall_clock − event_timestamp (ms)",
                vec![100.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 30000.0, 60000.0]
            ),
            event_age_latest_s: gauge!(
                "routers_event_age_latest_s",
                "Age of the most recently processed event (s); exact value, safe for replay data"
            ),
            registry,
        }
    })
}

pub async fn serve(addr: SocketAddr) {
    let app = Router::new().route("/metrics", get(render));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    log::info!("metrics listening on http://{addr}/metrics");
    axum::serve(listener, app).await.unwrap();
}

async fn render() -> String {
    let encoder = TextEncoder::new();
    let families = global().registry.gather();
    encoder.encode_to_string(&families).unwrap_or_default()
}
