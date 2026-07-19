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

use std::time::Duration;

use web_time::{SystemTime, UNIX_EPOCH};

use async_nats::HeaderMap;
use opentelemetry::propagation::{Extractor, Injector};
use opentelemetry::trace::{Span, Tracer, TracerProvider};
use opentelemetry::{Context, KeyValue, global};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Millisecond wall-clock send time; `traceparent` alone carries no times,
/// so queue wait needs this one extra header.
const SENT_AT: &str = "x-routers-sent-at-ms";

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

    if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
        headers.insert(SENT_AT, now.as_millis().to_string().as_str());
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
