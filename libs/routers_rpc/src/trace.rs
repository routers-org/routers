//! Creates the OpenTelemetry exporter using
//! `tracing_subscriber`, and batching requests
//! to the output defined by the environment.
//!
//! An example environment is shown:
//! ```bash
//! OTEL_EXPORTER_OTLP_ENDPOINT=https://<exporter>.com
//! OTEL_EXPORTER_OTLP_HEADERS=<api-key> [if required]
//! OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
//! OTEL_SERVICE_NAME=routers
//! ```

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry};

pub struct Tracer;

impl Tracer {
    /// Initialises the tracer, using tracing subscription.
    /// This is optional, not calling this function will simply
    /// not log traces.
    pub fn init() {
        let formatting = tracing_subscriber::fmt::layer()
            .with_thread_ids(true) // include the thread ID of the current thread
            .with_thread_names(true) // include the name of the current thread
            .compact();

        // Initialise tracing with subscribers and environment filter
        Registry::default()
            .with(EnvFilter::from_default_env())
            .with(formatting)
            .init()
    }
}
