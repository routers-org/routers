//! Trace context over the NATS hop, carried in message headers so the event
//! payloads stay untouched.
//!
//! On publish, the sink stamps the message with the W3C `traceparent` of the
//! span it was sent under, plus the wall-clock send time. On receipt, the
//! stream closes the loop by emitting a `queue_wait` span *backdated to the
//! send time* — so the time a message spends sitting in NATS becomes an
//! ordinary span, and therefore an ordinary latency histogram once the
//! collector's spanmetrics aggregates it. No hand-kept metric registry, and
//! no timestamps in the event structs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use web_time::{SystemTime, UNIX_EPOCH};

/// The stamp must be a real wall clock — it crosses process boundaries.
/// `web_time` re-exports std on native targets, so the wasm lint still
/// resolves to the disallowed method; these binaries are native-only.
#[allow(clippy::disallowed_methods)]
fn now() -> SystemTime {
    SystemTime::now()
}

use async_nats::HeaderMap;
use opentelemetry::propagation::{Extractor, Injector};
use opentelemetry::trace::{Span, Tracer, TracerProvider};
use opentelemetry::{Context, KeyValue, global};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Millisecond wall-clock send time; `traceparent` alone carries no times,
/// so queue wait needs this one extra header.
const SENT_AT: &str = "x-routers-sent-at-ms";

/// The send stamp of the most recently yielded message (ms since epoch;
/// 0 = none seen). An ambient slot rather than part of the stream's item
/// type, so consumers that don't care never see it. Sound because every
/// service consumes messages one at a time: between a stream yielding an
/// item and the loop body reading this, nothing else can have been yielded.
static LAST_SENT_AT: AtomicU64 = AtomicU64::new(0);

/// When the message most recently yielded by any [`NATSStream`] in this
/// process was published, per its wire stamp. Lets a consumer correlate
/// walltimes across services — e.g. the orchestrator measuring raw-event →
/// matched-result — without the event structs carrying timestamps.
pub fn last_sent_at() -> Option<SystemTime> {
    match LAST_SENT_AT.load(Ordering::Relaxed) {
        0 => None,
        millis => Some(UNIX_EPOCH + Duration::from_millis(millis)),
    }
}

/// Emit a span covering an arbitrary wall-clock interval — the primitive
/// behind cross-service walltime metrics: hand it two wire stamps and the
/// collector's spanmetrics does the rest. A no-op without an OTLP provider,
/// and inverted intervals (skewed clocks, out-of-order arrival) are ignored
/// rather than recorded as nonsense.
pub fn span_between(name: &'static str, start: SystemTime, end: SystemTime) {
    if end < start {
        return;
    }

    let tracer = global::tracer_provider().tracer("routers_realtime");
    tracer
        .span_builder(name)
        .with_start_time(start)
        .start(&tracer)
        .end_with_timestamp(end);
}

struct Headers<'a>(&'a mut HeaderMap);

impl Injector for Headers<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key, value.as_str());
    }
}

struct HeadersRef<'a>(&'a HeaderMap);

impl Extractor for HeadersRef<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|value| value.as_str())
    }

    fn keys(&self) -> Vec<&str> {
        // The propagator only ever looks up known keys; enumeration is unused.
        Vec::new()
    }
}

/// Headers for an outbound message: the current span's context (W3C
/// `traceparent`) and the send time.
pub(super) fn outbound() -> HeaderMap {
    let mut headers = HeaderMap::new();

    let context = tracing::Span::current().context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut Headers(&mut headers))
    });

    if let Ok(since_epoch) = now().duration_since(UNIX_EPOCH) {
        headers.insert(SENT_AT, since_epoch.as_millis().to_string().as_str());
    }

    headers
}

/// Record the queue wait of an inbound message: a span covering send → now,
/// parented to the publisher's trace. A no-op (`~ns`) when no OTLP provider
/// is installed.
pub(super) fn inbound(subject: &str, headers: Option<&HeaderMap>) {
    let Some(headers) = headers else { return };
    let Some(sent_at) = headers
        .get(SENT_AT)
        .and_then(|value| value.as_str().parse::<u64>().ok())
    else {
        return;
    };

    LAST_SENT_AT.store(sent_at, Ordering::Relaxed);

    let parent = global::get_text_map_propagator(|propagator| {
        propagator.extract_with_context(&Context::current(), &HeadersRef(headers))
    });

    let tracer = global::tracer_provider().tracer("routers_realtime");
    tracer
        .span_builder("queue_wait")
        .with_start_time(UNIX_EPOCH + Duration::from_millis(sent_at))
        .with_attributes([KeyValue::new("subject", subject.to_string())])
        .start_with_context(&tracer, &parent)
        .end();
}

/// Count a message the stream had to discard, as a zero-duration marker
/// span — spanmetrics turns it into a plain counter.
pub(super) fn dropped(subject: &str, reason: &'static str) {
    let tracer = global::tracer_provider().tracer("routers_realtime");
    tracer
        .span_builder("bus_drop")
        .with_attributes([
            KeyValue::new("subject", subject.to_string()),
            KeyValue::new("reason", reason),
        ])
        .start(&tracer)
        .end();
}
