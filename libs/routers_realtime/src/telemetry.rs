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

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
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

            let provider = SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
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

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().compact())
        .with(export)
        .init();

    Telemetry { provider }
}
