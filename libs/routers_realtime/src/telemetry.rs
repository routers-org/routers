//! Tracing for the realtime binaries: human-readable logs always, OTLP span
//! export when configured.
//!
//! The only developer-facing surface is the `tracing` macros —
//! `#[instrument]`, `info!`, `info_span!` — everything here is plumbing.
//! Spans become Prometheus metrics downstream: the devstack's
//! otel-collector aggregates every span into duration histograms and call
//! counters (spanmetrics), so no metric registry lives in the application.
//!
//! Export is driven entirely by the standard OTLP environment:
//!
//! ```bash
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318   # omit to disable
//! RUST_LOG=info                                            # filters logs AND exported spans
//! ```

use std::time::Duration;

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{BatchConfigBuilder, SdkTracerProvider};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Keeps the OTLP pipeline alive; dropping it flushes any batched spans.
/// Bind it in `main` — `let _telemetry = telemetry::init("matcher");`.
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take()
            && let Err(err) = provider.shutdown()
        {
            eprintln!("telemetry shutdown: {err}");
        }
    }
}

/// Install the global subscriber: an `EnvFilter`ed compact log formatter,
/// plus an OTLP span exporter when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
/// Without the endpoint this degrades to plain structured logging, so local
/// runs need no collector.
pub fn init(service: &'static str) -> Telemetry {
    // The W3C `traceparent` propagator is what lets a trace continue across
    // the NATS hop (see `bus::nats`). Global, so the bus layer never needs a
    // handle threaded through to it.
    global::set_text_map_propagator(TraceContextPropagator::new());

    let provider = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .is_ok()
        .then(|| {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .build()
                .expect("OTLP exporter builds from its environment");

            // Fire & forget: exports run as spawned tokio tasks (async
            // reqwest), never blocking the hot path — a full queue drops
            // spans rather than applying backpressure. Flushed twice a
            // second with headroom for ~10k spans/s bursts; metrics lag the
            // pipeline, they must never throttle it. Requires a running
            // tokio runtime, so call `init` from within `#[tokio::main]`.
            let processor = BatchSpanProcessor::builder(exporter, runtime::Tokio)
                .with_batch_config(
                    BatchConfigBuilder::default()
                        .with_scheduled_delay(Duration::from_millis(500))
                        .with_max_queue_size(16_384)
                        .with_max_export_batch_size(2_048)
                        .build(),
                )
                .build();

            let provider = SdkTracerProvider::builder()
                .with_span_processor(processor)
                .with_resource(Resource::builder().with_service_name(service).build())
                .build();

            // Bus-level spans (queue-wait) are created through the global
            // provider, not the tracing bridge.
            global::set_tracer_provider(provider.clone());
            provider
        });

    // `Option<Layer>` is itself a `Layer`, so one registry serves both modes.
    let export = provider
        .as_ref()
        .map(|provider| tracing_opentelemetry::layer().with_tracer(provider.tracer(service)));

    // Quieten the export path's own chatter: at RUST_LOG=debug the exporter
    // logs several lines per batch (hyper pools, reqwest sends), which at
    // pipeline rates is itself a throughput tax. Explicit RUST_LOG
    // directives for these targets still override.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"))
        .add_directive("opentelemetry=warn".parse().expect("static directive"))
        .add_directive("opentelemetry_sdk=warn".parse().expect("static directive"))
        .add_directive("opentelemetry-otlp=warn".parse().expect("static directive"))
        .add_directive("hyper_util=warn".parse().expect("static directive"))
        .add_directive("reqwest=warn".parse().expect("static directive"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().compact())
        .with(export)
        .init();

    Telemetry { provider }
}
