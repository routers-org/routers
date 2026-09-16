//! Tracing for the realtime binaries: human-readable logs always, OTLP span
//! export when configured.
//!
//! The only developer-facing surface is the `tracing` macros —
//! `#[instrument]`, `info!`, `info_span!` — plus the bounded-label
//! [`Metrics`](crate::metrics::Metrics) handle. Spans carry per-request detail
//! (vehicle and job ids), while the meter this module installs carries the
//! bounded-cardinality aggregates: the two are exported side by side over OTLP.
//!
//! Export is driven entirely by the standard OTLP environment:
//!
//! ```bash
//! OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318   # omit to disable
//! RUST_LOG=info                                            # filters logs AND exported spans
//! ```
//!
//! # Metric export and the OTLP client
//!
//! Both signals export off the Tokio reactor. Spans go through the async-runtime
//! batch processor (`runtime::Tokio`); metrics go through the async-runtime
//! [`PeriodicReader`] (the SDK's
//! `experimental_metrics_periodicreader_with_async_runtime` feature). The
//! thread-based reader would drive its exporter with a blocking `block_on`,
//! which the async `reqwest` client (`reqwest-client` feature) cannot service
//! off a Tokio reactor — a real collector export would stall. Scheduling the
//! collect-and-export as Tokio tasks instead keeps the metric path robust. The
//! no-endpoint path (all tests, local runs) is unaffected: the global meter is a
//! no-op and nothing exports.

use core::time::Duration;

use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{BatchConfigBuilder, SdkTracerProvider};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Keeps the OTLP pipeline alive; dropping it flushes any batched spans and the
/// last metrics collection. Bind it in `main` —
/// `let _telemetry = telemetry::init("matcher");`.
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take()
            && let Err(err) = provider.shutdown()
        {
            eprintln!("telemetry shutdown: {err}");
        }
        // Flush the final metrics collection before the reader's thread stops.
        if let Some(provider) = self.meter_provider.take()
            && let Err(err) = provider.shutdown()
        {
            eprintln!("meter shutdown: {err}");
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

    // The bounded-label meter shares the same OTLP endpoint. When it is set we
    // install a `PeriodicReader` that collects and exports every 10 s (metrics
    // are aggregates, so a coarse cadence is fine) and register it globally so
    // `Metrics::new` binds to it. Absent the endpoint the global meter stays a
    // no-op and every `Metrics` instrument is free. The reader schedules its
    // collect-and-export as tokio tasks (see the module-level note), so it needs
    // a running runtime — call `init` from within `#[tokio::main]`.
    let meter_provider = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .is_ok()
        .then(|| {
            let exporter = opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .build()
                .expect("OTLP metric exporter builds from its environment");

            // Drive the reader on the tokio runtime, mirroring the span side's
            // async batch processor. The thread-based `PeriodicReader` collects
            // and exports with a `block_on` off the reactor, which the async
            // `reqwest-client` OTLP exporter cannot service; the async-runtime
            // reader schedules its collect+export as tokio tasks instead.
            let reader = PeriodicReader::builder(exporter, runtime::Tokio)
                .with_interval(Duration::from_secs(10))
                .build();

            let provider = SdkMeterProvider::builder()
                .with_reader(reader)
                .with_resource(Resource::builder().with_service_name(service).build())
                .build();

            global::set_meter_provider(provider.clone());
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

    Telemetry {
        provider,
        meter_provider,
    }
}
