//! Internal ingress: validation, idempotent publication, and isolated replay.
//!
//! Ingress is the seam where raw observations enter the raw journal.
//! [`validate`] rejects observations a solve could never use; every publish
//! carries a deterministic `Nats-Msg-Id` ([`msg_id`]) so a retried send is
//! deduped. [`Ingress::isolated`] prefixes subjects with `replay.<run>.` onto
//! their own streams so replay never touches the live path. Per-vehicle order
//! holds only if a caller publishes one vehicle's observations in order.

use core::future::IntoFuture;
use core::time::Duration;

use anyhow::anyhow;
use async_nats::HeaderMap;
use async_nats::jetstream::{
    self,
    stream::{Config, DiscardPolicy, RetentionPolicy, StorageType},
};
use chrono::{DateTime, TimeDelta, Utc};
use thiserror::Error;
use web_time::SystemTime;

use crate::bus::{self, Wire};
use crate::event::Payload;
use crate::partition::{self, PARTITIONS};
use crate::protocol::ids::{IdError, headers, token_safe};
use crate::topology::{DUPLICATE_WINDOW, RawConfig, ensure_raw_stream, raw_subject};

/// The inclusive latitude bound in degrees; anything outside is off the globe.
const LATITUDE_LIMIT: f64 = 90.0;
/// The inclusive longitude bound in degrees.
const LONGITUDE_LIMIT: f64 = 180.0;

/// The publish acknowledgement timeout used when a caller does not tune one.
const DEFAULT_PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
/// How many times an ambiguous publish is retried before it is failed.
const DEFAULT_ATTEMPTS: u32 = 5;
/// The first inter-attempt backoff; it doubles up to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_millis(50);
/// The ceiling on the exponential publish backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// How stale or how early an observation may be to still be admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngressLimits {
    /// The oldest an observation may be; older is [`IngressError::TooOld`].
    pub max_age: Duration,
    /// How far into the future a timestamp may sit before [`IngressError::InFuture`].
    pub max_ahead: Duration,
}

impl Default for IngressLimits {
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(7 * 24 * 60 * 60),
            max_ahead: Duration::from_secs(5 * 60),
        }
    }
}

/// Why an observation was not admitted. Validation variants are data faults;
/// [`IngressError::Publish`] is an infrastructure fault after retries.
#[derive(Debug, Error)]
pub enum IngressError {
    /// The vehicle id was zero, which no real vehicle hashes to.
    #[error("vehicle id is zero")]
    ZeroVehicle,
    /// A coordinate was `NaN` or infinite, so it cannot be placed on the map.
    #[error("coordinate is not finite")]
    NonFinite,
    /// The latitude fell outside `[-90, 90]` degrees.
    #[error("latitude {0} is outside [-90, 90]")]
    LatitudeOutOfRange(f64),
    /// The longitude fell outside `[-180, 180]` degrees.
    #[error("longitude {0} is outside [-180, 180]")]
    LongitudeOutOfRange(f64),
    /// The observation was older than [`IngressLimits::max_age`].
    #[error("observation is {age:?} old, past the age limit")]
    TooOld {
        /// How far in the past the observation's timestamp was.
        age: Duration,
    },
    /// The timestamp was more than [`IngressLimits::max_ahead`] into the future.
    #[error("observation timestamp is {ahead:?} in the future, past the skew limit")]
    InFuture {
        /// How far in the future the observation's timestamp was.
        ahead: Duration,
    },
    /// The broker never acknowledged the publish, after every retry.
    #[error("publish was not acknowledged after retries")]
    Publish(#[source] anyhow::Error),
}

impl IngressError {
    /// A bounded, stable label for this error, for tallying rejections by variant.
    pub fn kind(&self) -> &'static str {
        match self {
            IngressError::ZeroVehicle => "zero_vehicle",
            IngressError::NonFinite => "non_finite",
            IngressError::LatitudeOutOfRange(_) => "latitude_out_of_range",
            IngressError::LongitudeOutOfRange(_) => "longitude_out_of_range",
            IngressError::TooOld { .. } => "too_old",
            IngressError::InFuture { .. } => "in_future",
            IngressError::Publish(_) => "publish",
        }
    }

    /// Whether this rejection is a single bad observation rather than a broker failure.
    pub fn is_data_fault(&self) -> bool {
        !matches!(self, IngressError::Publish(_))
    }
}

/// Check one observation against the admission rules, using `now` as the
/// reference clock and `limits` as the age/skew window.
pub fn validate(
    payload: &Payload,
    now: DateTime<Utc>,
    limits: &IngressLimits,
) -> Result<(), IngressError> {
    if payload.vehicle_id.0 == 0 {
        return Err(IngressError::ZeroVehicle);
    }

    let longitude = payload.point.x();
    let latitude = payload.point.y();
    if !longitude.is_finite() || !latitude.is_finite() {
        return Err(IngressError::NonFinite);
    }
    if !(-LATITUDE_LIMIT..=LATITUDE_LIMIT).contains(&latitude) {
        return Err(IngressError::LatitudeOutOfRange(latitude));
    }
    if !(-LONGITUDE_LIMIT..=LONGITUDE_LIMIT).contains(&longitude) {
        return Err(IngressError::LongitudeOutOfRange(longitude));
    }

    // A limit too large for `TimeDelta` saturates to "never reject on time".
    let max_age = TimeDelta::from_std(limits.max_age).unwrap_or(TimeDelta::MAX);
    let max_ahead = TimeDelta::from_std(limits.max_ahead).unwrap_or(TimeDelta::MAX);
    let delta = now - payload.timestamp; // positive => in the past
    if delta > max_age {
        return Err(IngressError::TooOld {
            age: delta.to_std().unwrap_or(Duration::ZERO),
        });
    }
    if -delta > max_ahead {
        return Err(IngressError::InFuture {
            ahead: (-delta).to_std().unwrap_or(Duration::ZERO),
        });
    }

    Ok(())
}

/// The raw-journal dedup key (`Nats-Msg-Id`) for an observation: `<vehicle_id>:<ts_us>`.
pub fn msg_id(payload: &Payload) -> String {
    format!(
        "{}:{}",
        payload.vehicle_id,
        payload.timestamp.timestamp_micros()
    )
}

/// The successful outcome of a publish: where the observation landed and whether it was a duplicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublishAck {
    /// The stream sequence assigned — the observation's revision downstream.
    pub sequence: u64,
    /// `true` if the broker collapsed this into an earlier identical publish.
    pub duplicate: bool,
}

/// A publisher of raw observations onto the journal, live or isolated.
///
/// Construct with [`Ingress::live`] or [`Ingress::isolated`], provision streams
/// once with [`Ingress::ensure_streams`], then [`Ingress::publish`] observations.
pub struct Ingress {
    js: jetstream::Context,
    /// The isolated run token, or `None` for the live journal.
    run: Option<String>,
    limits: IngressLimits,
    publish_timeout: Duration,
    attempts: u32,
}

impl Ingress {
    /// An ingress onto the live raw journal (canonical subjects and streams).
    pub fn live(js: jetstream::Context, limits: IngressLimits) -> Self {
        Self {
            js,
            run: None,
            limits,
            publish_timeout: DEFAULT_PUBLISH_TIMEOUT,
            attempts: DEFAULT_ATTEMPTS,
        }
    }

    /// An ingress onto an isolated replay journal named `run`.
    ///
    /// `run` must be NATS-safe; an unsafe or empty token is rejected.
    pub fn isolated(
        js: jetstream::Context,
        run: &str,
        limits: IngressLimits,
    ) -> Result<Self, IdError> {
        if run.is_empty() {
            return Err(IdError::Empty);
        }
        if !token_safe(run) {
            return Err(IdError::UnsafeToken(run.to_owned()));
        }
        Ok(Self {
            js,
            run: Some(run.to_owned()),
            limits,
            publish_timeout: DEFAULT_PUBLISH_TIMEOUT,
            attempts: DEFAULT_ATTEMPTS,
        })
    }

    /// Override the per-attempt publish-acknowledgement timeout.
    pub fn with_publish_timeout(mut self, timeout: Duration) -> Self {
        self.publish_timeout = timeout;
        self
    }

    /// Override how many times an ambiguous publish is retried (at least once).
    pub fn with_attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts.max(1);
        self
    }

    /// The admission limits this ingress applies.
    pub fn limits(&self) -> &IngressLimits {
        &self.limits
    }

    /// The subject one partition's observations publish to, prefixed for an isolated run.
    pub fn subject(&self, partition: u64) -> String {
        prefixed_raw_subject(self.run.as_deref(), partition)
    }

    /// Provision the raw streams this ingress publishes onto, idempotently.
    pub async fn ensure_streams(&self, streams: u64, cfg: &RawConfig) -> anyhow::Result<()> {
        match &self.run {
            None => {
                for index in 0..streams {
                    ensure_raw_stream(&self.js, index, streams, cfg).await?;
                }
            }
            Some(run) => {
                for index in 0..streams {
                    self.ensure_isolated_stream(run, index, streams, cfg)
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Create (or reuse) one isolated replay stream over the prefixed subjects of stream `index`.
    async fn ensure_isolated_stream(
        &self,
        run: &str,
        index: u64,
        streams: u64,
        cfg: &RawConfig,
    ) -> anyhow::Result<jetstream::stream::Stream> {
        let chunk = PARTITIONS.div_ceil(streams);
        let partitions = (index * chunk)..(((index + 1) * chunk).min(PARTITIONS));
        let subjects = partitions
            .map(|partition| self.subject(partition))
            .collect();

        self.js
            .get_or_create_stream(Config {
                name: format!("REPLAY-{run}-RAW-{index}"),
                subjects,
                retention: RetentionPolicy::Limits,
                storage: StorageType::File,
                max_age: cfg.max_age,
                discard: DiscardPolicy::Old,
                duplicate_window: DUPLICATE_WINDOW,
                ..Default::default()
            })
            .await
            .map_err(|error| anyhow!("could not create replay stream {run}/{index}: {error}"))
    }

    /// Validate, stamp, and publish one observation, returning its acknowledgement.
    ///
    /// An ambiguous send is retried under the same `msg_id`; a validation failure
    /// returns immediately and exhausted retries return [`IngressError::Publish`].
    pub async fn publish(
        &self,
        payload: &Payload,
        received_at: SystemTime,
    ) -> Result<PublishAck, IngressError> {
        // The receipt time is this observation's clock; validate against it.
        validate(payload, DateTime::<Utc>::from(received_at), &self.limits)?;

        let subject = self.subject(partition::partition_of(payload.vehicle_id));
        let id = msg_id(payload);
        let bytes = payload.encode().map_err(IngressError::Publish)?;

        let mut headers = bus::outbound();
        headers::stamp_schema(&mut headers);
        headers::stamp_received_at(&mut headers, received_at);
        headers::stamp_msg_id(&mut headers, &id);

        let mut backoff = INITIAL_BACKOFF;
        let mut last: Option<anyhow::Error> = None;
        for attempt in 0..self.attempts.max(1) {
            if attempt > 0 {
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            match self.send(&subject, headers.clone(), bytes.clone()).await {
                Ok(ack) => return Ok(ack),
                Err(error) => last = Some(error),
            }
        }

        Err(IngressError::Publish(
            last.unwrap_or_else(|| anyhow!("no publish attempt was made")),
        ))
    }

    /// One publish send-and-await under [`Ingress::publish_timeout`]; a timeout is ambiguous.
    async fn send(
        &self,
        subject: &str,
        headers: HeaderMap,
        bytes: Vec<u8>,
    ) -> anyhow::Result<PublishAck> {
        let pending = self
            .js
            .publish_with_headers(subject.to_owned(), headers, bytes.into())
            .await
            .map_err(|error| anyhow!("publish send failed: {error}"))?;

        let ack = tokio::time::timeout(self.publish_timeout, pending.into_future())
            .await
            .map_err(|_| {
                anyhow!(
                    "publish acknowledgement timed out after {:?}",
                    self.publish_timeout
                )
            })?
            .map_err(|error| anyhow!("publish acknowledgement failed: {error}"))?;

        Ok(PublishAck {
            sequence: ack.sequence,
            duplicate: ack.duplicate,
        })
    }
}

/// The raw subject for `partition`, prefixed `replay.<run>.` when `run` is set.
fn prefixed_raw_subject(run: Option<&str>, partition: u64) -> String {
    match run {
        Some(run) => format!("replay.{run}.{}", raw_subject(partition)),
        None => raw_subject(partition),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use geo::Point;

    use super::*;
    use crate::event::VehicleId;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_micros(1_775_000_000_000_000).unwrap()
    }

    fn payload(vehicle: u64, lon: f64, lat: f64, ts: DateTime<Utc>) -> Payload {
        Payload {
            vehicle_id: VehicleId(vehicle),
            timestamp: ts,
            point: Point::new(lon, lat),
        }
    }

    fn valid() -> Payload {
        payload(42, 151.2093, -33.8688, now())
    }

    #[test]
    fn validation_table() {
        let limits = IngressLimits::default();
        let old = now() - TimeDelta::days(8); // past max_age (7d)
        let future = now() + TimeDelta::minutes(6); // past max_ahead (5m)

        let cases: [(&str, Payload, Option<&str>); 9] = [
            ("valid", valid(), None),
            (
                "valid at exact age boundary",
                payload(42, 151.2093, -33.8688, now() - TimeDelta::days(7)),
                None,
            ),
            (
                "zero vehicle",
                payload(0, 151.2093, -33.8688, now()),
                Some("zero_vehicle"),
            ),
            (
                "nan latitude",
                payload(42, 151.2093, f64::NAN, now()),
                Some("non_finite"),
            ),
            (
                "infinite longitude",
                payload(42, f64::INFINITY, -33.8688, now()),
                Some("non_finite"),
            ),
            (
                "latitude too high",
                payload(42, 151.2093, 90.5, now()),
                Some("latitude_out_of_range"),
            ),
            (
                "longitude too low",
                payload(42, -180.5, -33.8688, now()),
                Some("longitude_out_of_range"),
            ),
            (
                "too old",
                payload(42, 151.2093, -33.8688, old),
                Some("too_old"),
            ),
            (
                "in the future",
                payload(42, 151.2093, -33.8688, future),
                Some("in_future"),
            ),
        ];

        for (label, event, expected) in cases {
            let result = validate(&event, now(), &limits);
            match expected {
                None => assert!(result.is_ok(), "{label}: expected accept, got {result:?}"),
                Some(kind) => {
                    let error = result.expect_err(&format!("{label}: expected rejection"));
                    assert_eq!(error.kind(), kind, "{label}");
                }
            }
        }
    }

    #[test]
    fn validation_carries_the_offending_value() {
        match validate(
            &payload(42, 151.2093, 90.5, now()),
            now(),
            &IngressLimits::default(),
        ) {
            Err(IngressError::LatitudeOutOfRange(lat)) => assert_eq!(lat, 90.5),
            other => panic!("expected LatitudeOutOfRange, got {other:?}"),
        }
        match validate(
            &payload(42, -181.0, -33.0, now()),
            now(),
            &IngressLimits::default(),
        ) {
            Err(IngressError::LongitudeOutOfRange(lon)) => assert_eq!(lon, -181.0),
            other => panic!("expected LongitudeOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn validation_reports_how_stale_or_early() {
        let limits = IngressLimits::default();
        match validate(
            &payload(42, 151.0, -33.0, now() - TimeDelta::days(10)),
            now(),
            &limits,
        ) {
            Err(IngressError::TooOld { age }) => {
                assert_eq!(age, Duration::from_secs(10 * 24 * 60 * 60));
            }
            other => panic!("expected TooOld, got {other:?}"),
        }
        match validate(
            &payload(42, 151.0, -33.0, now() + TimeDelta::minutes(10)),
            now(),
            &limits,
        ) {
            Err(IngressError::InFuture { ahead }) => {
                assert_eq!(ahead, Duration::from_secs(10 * 60));
            }
            other => panic!("expected InFuture, got {other:?}"),
        }
    }

    #[test]
    fn is_data_fault_separates_bad_rows_from_broker_failures() {
        assert!(IngressError::ZeroVehicle.is_data_fault());
        assert!(
            IngressError::TooOld {
                age: Duration::ZERO
            }
            .is_data_fault()
        );
        assert!(!IngressError::Publish(anyhow!("broker down")).is_data_fault());
    }

    #[test]
    fn msg_id_is_vehicle_and_micros() {
        let event = payload(
            42,
            151.0,
            -33.0,
            Utc.timestamp_micros(1_775_000_000_123_456).unwrap(),
        );
        assert_eq!(msg_id(&event), "42:1775000000123456");
    }

    #[test]
    fn msg_id_distinguishes_observations_but_not_retries() {
        let event = valid();
        assert_eq!(msg_id(&event), msg_id(&valid()));
        let later = payload(42, 151.2093, -33.8688, now() + TimeDelta::seconds(1));
        assert_ne!(msg_id(&event), msg_id(&later));
    }

    #[test]
    fn subjects_are_bare_when_live() {
        for partition in [0u64, 1, 511, PARTITIONS - 1] {
            assert_eq!(
                prefixed_raw_subject(None, partition),
                raw_subject(partition),
                "live subject must not be prefixed",
            );
        }
    }

    #[test]
    fn subjects_are_prefixed_when_isolated() {
        assert_eq!(
            prefixed_raw_subject(Some("run-7"), 485),
            "replay.run-7.events.raw.p.485",
        );
        assert_eq!(
            crate::topology::partition_of_subject(&prefixed_raw_subject(Some("run-7"), 485)),
            Some(485),
        );
    }

    #[test]
    fn isolated_rejects_unsafe_run_tokens() {
        assert!(token_safe("run-7"));
        assert!(token_safe("backfill_2026"));
        for bad in ["", "run 7", "run.7", "run*", "run>", "réplay"] {
            assert!(!token_safe(bad), "{bad:?} should be rejected");
        }
    }
}
