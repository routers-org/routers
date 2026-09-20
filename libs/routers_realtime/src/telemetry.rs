//! Tracing for the realtime binaries: compact logs always, OTLP span and metric
//! export when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. `RUST_LOG` filters both.

use core::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{KeyValue, global};
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

/// Keeps the OTLP pipeline alive; dropping it flushes batched spans and metrics.
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
        if let Some(provider) = self.meter_provider.take()
            && let Err(err) = provider.shutdown()
        {
            eprintln!("meter shutdown: {err}");
        }
    }
}

/// Install the global subscriber; without an OTLP endpoint this is plain logging.
/// Call from within a tokio runtime: the exporters run as spawned tasks.
pub fn init(service: &'static str) -> Telemetry {
    global::set_text_map_propagator(TraceContextPropagator::new());

    // Prometheus identifies cumulative OTLP counters by their resource. Without
    // an instance id, replicas (and replacement pods) collapse into one series,
    // making counter resets look like impossible throughput spikes.
    let instance = std::env::var("POD_NAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| format!("pid-{}", std::process::id()));
    let resource = Resource::builder()
        .with_service_name(service)
        .with_attribute(KeyValue::new("service.instance.id", instance))
        .build();

    let provider = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .is_ok()
        .then(|| {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .build()
                .expect("OTLP exporter builds from its environment");

            // A full queue drops spans; export must never back-pressure the pipeline.
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
                .with_resource(resource.clone())
                .build();

            global::set_tracer_provider(provider.clone());
            provider
        });

    let meter_provider = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .is_ok()
        .then(|| {
            let exporter = opentelemetry_otlp::MetricExporter::builder()
                .with_http()
                .build()
                .expect("OTLP metric exporter builds from its environment");

            // The thread-based reader would `block_on` off the reactor and deadlock the async exporter.
            let reader = PeriodicReader::builder(exporter, runtime::Tokio)
                .with_interval(Duration::from_secs(10))
                .build();

            let provider = SdkMeterProvider::builder()
                .with_reader(reader)
                .with_resource(resource)
                .build();

            global::set_meter_provider(provider.clone());
            provider
        });

    let export = provider
        .as_ref()
        .map(|provider| tracing_opentelemetry::layer().with_tracer(provider.tracer(service)));

    // The exporter's own debug logging is a throughput tax; explicit RUST_LOG directives still win.
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
