use axum::{Router, routing::get};
use prometheus::{Gauge, Histogram, HistogramOpts, IntCounter, Registry, TextEncoder, opts};
use std::net::SocketAddr;
use std::sync::OnceLock;

// ── Matcher metrics ───────────────────────────────────────────────────────────

pub struct MatcherMetrics {
    /// Wall-clock time from NATS delivery to match completion (ms).
    pub match_latency_ms: Histogram,
    /// How long network.r#match() itself takes inside the HMM solver (ms).
    pub solve_latency_ms: Histogram,
    pub matches_success: IntCounter,
    pub matches_no_candidate: IntCounter,
    pub matches_error: IntCounter,
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

        MatcherMetrics {
            match_latency_ms: histogram!(
                "routers_match_latency_ms",
                "End-to-end match latency: NATS delivery → result published (ms)",
                lat.clone()
            ),
            solve_latency_ms: histogram!(
                "routers_solve_latency_ms",
                "HMM solver wall time (ms)",
                lat.clone()
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
