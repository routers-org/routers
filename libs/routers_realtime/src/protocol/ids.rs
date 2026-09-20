//! Identities and versions shared by every plane. The newtypes here are
//! distinct types so a sequence can never be handed where a revision is meant,
//! and values that become NATS tokens or dedup keys are validated at
//! construction and stay well-formed thereafter.

use core::fmt;
use core::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The wire schema this build speaks, stamped onto every message.
pub const SCHEMA_VERSION: SchemaVersion = SchemaVersion(1);

/// Something a value could not become because it would not be safe on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum IdError {
    /// The token held a character outside `[A-Za-z0-9_-]`.
    #[error("token is not NATS-safe (needs [A-Za-z0-9_-]+): {0:?}")]
    UnsafeToken(String),
    /// The token was empty; an empty subject segment is not addressable.
    #[error("token is empty")]
    Empty,
    /// A hex identity was not 32 hexadecimal characters.
    #[error("not 32 hex characters: {0:?}")]
    BadHex(String),
}

/// Declare a fixed-size integer identity: a transparent newtype that prints as
/// its inner value.
macro_rules! plain_id {
    ($(#[$meta:meta])* $name:ident($inner:ty)) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(pub $inner);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

plain_id! {
    /// The wire-contract version a message was produced against.
    SchemaVersion(u32)
}

plain_id! {
    /// A committed decision's ordinal: the raw stream sequence of the
    /// observation that triggered it. Higher revisions supersede lower ones for
    /// the same (vehicle, timestamp).
    Revision(u64)
}

plain_id! {
    /// A continuity generation for one vehicle, valued as its opening
    /// observation's sequence so segments are deterministic without a shared
    /// counter.
    SegmentId(u64)
}

plain_id! {
    /// A priority lane within a region's solve-job plane; lane `0` is the default.
    Lane(u8)
}

impl Lane {
    /// The lane every job takes unless it is deliberately promoted.
    pub const DEFAULT: Lane = Lane(0);
}

impl Default for Lane {
    fn default() -> Self {
        Lane::DEFAULT
    }
}

/// A raw observation's durable identity: its JetStream stream sequence
/// qualified by partition, unique across the fleet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObservationId {
    pub partition: u16,
    pub sequence: u64,
}

impl fmt::Display for ObservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "p{}:{}", self.partition, self.sequence)
    }
}

impl From<ObservationId> for Revision {
    /// An observation's sequence is the revision it triggers.
    fn from(id: ObservationId) -> Self {
        Revision(id.sequence)
    }
}

impl From<ObservationId> for SegmentId {
    /// A fresh vehicle's first segment is valued as its opening observation's
    /// sequence.
    fn from(id: ObservationId) -> Self {
        SegmentId(id.sequence)
    }
}

/// Declare a 128-bit hash identity: prints and parses as 32 lowercase hex
/// characters.
macro_rules! hex_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub u128);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:032x}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(IdError::BadHex(s.to_owned()));
                }
                u128::from_str_radix(s, 16)
                    .map($name)
                    .map_err(|_| IdError::BadHex(s.to_owned()))
            }
        }
    };
}

hex_id! {
    /// Deterministic 128-bit job identity: the first 16 bytes of the SHA-256 of
    /// a job's `JobIdentity`, so an ambiguous re-publish carries the same id and dedups.
    JobId
}

hex_id! {
    /// Identity of one committed output, derived from its job so it too is
    /// deterministic and self-deduping.
    OutputId
}

impl OutputId {
    /// The output identity for a job: the first 16 bytes of
    /// `sha256(job_id_be_bytes ∥ b"output")`.
    pub fn for_job(job: JobId) -> OutputId {
        OutputId(digest128(&[
            job.0.to_be_bytes().as_slice(),
            b"output".as_slice(),
        ]))
    }
}

/// A NATS-token-safe newtype backed by a validated string matching
/// `[A-Za-z0-9_-]+`, so it is safe to interpolate into a subject.
macro_rules! token_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            /// Validate `s` as a NATS-safe token and wrap it. Rejects the
            /// empty string and anything outside `[A-Za-z0-9_-]+`.
            pub fn new(s: &str) -> Result<Self, IdError> {
                if s.is_empty() {
                    return Err(IdError::Empty);
                }
                if !token_safe(s) {
                    return Err(IdError::UnsafeToken(s.to_owned()));
                }
                Ok(Self(s.to_owned()))
            }

            /// Borrow the validated token.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

token_id! {
    /// Identifies the road-network snapshot a solve ran against; a NATS-safe subject token.
    GraphVersion
}

token_id! {
    /// Identifies a solve region (a shard grouping); a NATS-safe subject and stream token.
    RegionId
}

/// Whether `s` is safe to use as a single NATS subject token: non-empty and
/// made only of `[A-Za-z0-9_-]`.
pub fn token_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// SHA-256 over the concatenation of `parts`, keeping the first 16 bytes as a
/// big-endian `u128`. The shared basis for every deterministic identity here.
pub fn digest128(parts: &[&[u8]]) -> u128 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut head = [0u8; 16];
    head.copy_from_slice(&digest[..16]);
    u128::from_be_bytes(head)
}

/// Message-header names and the helpers that stamp and read them, centralised so
/// every plane spells them the same.
pub mod headers {
    use core::time::Duration;

    use async_nats::HeaderMap;
    use web_time::{SystemTime, UNIX_EPOCH};

    use super::{SCHEMA_VERSION, SchemaVersion};

    /// The producer's wire schema version (`u32`, decimal).
    pub const SCHEMA: &str = "x-routers-schema";
    /// When ingress received the raw observation (unix millis, decimal).
    pub const RECEIVED_AT_MS: &str = "x-routers-received-at-ms";
    /// When the message was published (unix millis); shared with `bus::trace`.
    pub const SENT_AT_MS: &str = "x-routers-sent-at-ms";
    /// The broker's dedup key (`Nats-Msg-Id`).
    pub const MSG_ID: &str = "Nats-Msg-Id";

    /// Stamp this build's [`SCHEMA_VERSION`] onto `headers`.
    pub fn stamp_schema(headers: &mut HeaderMap) {
        headers.insert(SCHEMA, SCHEMA_VERSION.0.to_string());
    }

    /// Read the schema version a peer stamped, if present and well-formed.
    pub fn schema_of(headers: &HeaderMap) -> Option<SchemaVersion> {
        headers
            .get(SCHEMA)?
            .as_str()
            .parse::<u32>()
            .ok()
            .map(SchemaVersion)
    }

    /// Set the broker dedup key (`Nats-Msg-Id`) to `id`.
    pub fn stamp_msg_id(headers: &mut HeaderMap, id: &str) {
        headers.insert(MSG_ID, id);
    }

    /// Read the broker dedup key, if present.
    pub fn msg_id_of(headers: &HeaderMap) -> Option<&str> {
        headers.get(MSG_ID).map(|value| value.as_str())
    }

    /// Stamp the ingress receipt time as unix millis; a time before the epoch records as `0`.
    pub fn stamp_received_at(headers: &mut HeaderMap, at: SystemTime) {
        let millis = at
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or(0);
        headers.insert(RECEIVED_AT_MS, millis.to_string());
    }

    /// Read the ingress receipt time, if present and well-formed.
    pub fn received_at_of(headers: &HeaderMap) -> Option<SystemTime> {
        let millis = headers.get(RECEIVED_AT_MS)?.as_str().parse::<u64>().ok()?;
        Some(UNIX_EPOCH + Duration::from_millis(millis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_id_round_trips() {
        let cases = [
            0u128,
            1,
            0xdead_beef,
            u128::MAX,
            0xe3b0_c442_98fc_1c14_9afb_f4c8_996f_b924,
        ];
        for raw in cases {
            let job = JobId(raw);
            let text = job.to_string();
            assert_eq!(text.len(), 32, "{text} should be 32 chars");
            assert_eq!(text, text.to_lowercase(), "{text} should be lowercase");
            assert_eq!(JobId::from_str(&text).unwrap(), job);
            assert_eq!(OutputId::from_str(&text).unwrap(), OutputId(raw));
        }
    }

    #[test]
    fn hex_id_display_is_zero_padded() {
        assert_eq!(JobId(1).to_string(), "00000000000000000000000000000001");
        assert_eq!(
            OutputId(u128::MAX).to_string(),
            "ffffffffffffffffffffffffffffffff"
        );
    }

    #[test]
    fn hex_id_from_str_rejects_malformed() {
        let bad = [
            "",                                  // empty
            "abc",                               // too short
            "e3b0c44298fc1c149afbf4c8996fb9245", // 33 chars
            "g3b0c44298fc1c149afbf4c8996fb924",  // non-hex digit
            " 3b0c44298fc1c149afbf4c8996fb924",  // leading space
        ];
        for text in bad {
            assert_eq!(
                JobId::from_str(text),
                Err(IdError::BadHex(text.to_owned())),
                "{text:?} should not parse"
            );
        }
    }

    #[test]
    fn digest128_known_answer() {
        let empty = digest128(&[]);
        assert_eq!(format!("{empty:032x}"), "e3b0c44298fc1c149afbf4c8996fb924");
        assert_eq!(JobId(empty).to_string(), "e3b0c44298fc1c149afbf4c8996fb924");
    }

    #[test]
    fn digest128_hashes_the_concatenation() {
        assert_eq!(
            digest128(&[b"foo".as_slice(), b"bar".as_slice()]),
            digest128(&[b"foobar".as_slice()])
        );
        assert_ne!(
            digest128(&[b"foobar".as_slice()]),
            digest128(&[b"foobaz".as_slice()])
        );
    }

    #[test]
    fn output_id_is_derived_and_stable() {
        let job = JobId(0xe3b0_c442_98fc_1c14_9afb_f4c8_996f_b924);
        let expected = OutputId(digest128(&[
            job.0.to_be_bytes().as_slice(),
            b"output".as_slice(),
        ]));
        assert_eq!(OutputId::for_job(job), expected);
        assert_eq!(OutputId::for_job(job), OutputId::for_job(job));
        assert_ne!(OutputId::for_job(job), OutputId::for_job(JobId(job.0 ^ 1)));
    }

    #[test]
    fn token_safety_table() {
        let cases = [
            ("europe", true),
            ("v1", true),
            ("graph_2024-09", true),
            ("A-Za-z0-9_", true),
            ("", false),
            ("has space", false),
            ("dotted.token", false),
            ("wild*", false),
            ("wild>", false),
            ("tab\there", false),
            ("unicodé", false),
        ];
        for (text, ok) in cases {
            assert_eq!(token_safe(text), ok, "token_safe({text:?})");
            assert_eq!(
                GraphVersion::new(text).is_ok(),
                ok,
                "GraphVersion::new({text:?})"
            );
            assert_eq!(RegionId::new(text).is_ok(), ok, "RegionId::new({text:?})");
        }
    }

    #[test]
    fn token_new_reports_why() {
        assert_eq!(GraphVersion::new(""), Err(IdError::Empty));
        assert_eq!(
            RegionId::new("bad.token"),
            Err(IdError::UnsafeToken("bad.token".to_owned()))
        );
        assert_eq!(GraphVersion::new("ok-1").unwrap().as_str(), "ok-1");
        assert_eq!(GraphVersion::new("ok-1").unwrap().to_string(), "ok-1");
    }

    #[test]
    fn observation_id_orders_by_partition_then_sequence() {
        let mut ids = [
            ObservationId {
                partition: 1,
                sequence: 5,
            },
            ObservationId {
                partition: 0,
                sequence: 9,
            },
            ObservationId {
                partition: 0,
                sequence: 2,
            },
            ObservationId {
                partition: 1,
                sequence: 1,
            },
        ];
        ids.sort();
        assert_eq!(
            ids,
            [
                ObservationId {
                    partition: 0,
                    sequence: 2
                },
                ObservationId {
                    partition: 0,
                    sequence: 9
                },
                ObservationId {
                    partition: 1,
                    sequence: 1
                },
                ObservationId {
                    partition: 1,
                    sequence: 5
                },
            ]
        );
    }

    #[test]
    fn observation_id_display_and_projections() {
        let id = ObservationId {
            partition: 7,
            sequence: 42,
        };
        assert_eq!(id.to_string(), "p7:42");
        assert_eq!(Revision::from(id), Revision(42));
        assert_eq!(SegmentId::from(id), SegmentId(42));
    }

    #[test]
    fn lane_defaults_to_zero() {
        assert_eq!(Lane::default(), Lane::DEFAULT);
        assert_eq!(Lane::DEFAULT, Lane(0));
        assert_eq!(Lane::default().to_string(), "0");
    }

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, SchemaVersion(1));
        assert_eq!(SCHEMA_VERSION.to_string(), "1");
    }

    #[test]
    fn headers_round_trip() {
        use async_nats::HeaderMap;
        use web_time::{Duration, UNIX_EPOCH};

        let mut map = HeaderMap::new();
        assert_eq!(headers::schema_of(&map), None);
        assert_eq!(headers::msg_id_of(&map), None);
        assert_eq!(headers::received_at_of(&map), None);

        headers::stamp_schema(&mut map);
        headers::stamp_msg_id(&mut map, "e3b0c44298fc1c149afbf4c8996fb924");
        let received = UNIX_EPOCH + Duration::from_millis(1_726_000_000_123);
        headers::stamp_received_at(&mut map, received);

        assert_eq!(headers::schema_of(&map), Some(SCHEMA_VERSION));
        assert_eq!(
            headers::msg_id_of(&map),
            Some("e3b0c44298fc1c149afbf4c8996fb924")
        );
        assert_eq!(headers::received_at_of(&map), Some(received));
    }

    #[test]
    fn header_names_are_stable() {
        assert_eq!(headers::SCHEMA, "x-routers-schema");
        assert_eq!(headers::RECEIVED_AT_MS, "x-routers-received-at-ms");
        assert_eq!(headers::SENT_AT_MS, "x-routers-sent-at-ms");
        assert_eq!(headers::MSG_ID, "Nats-Msg-Id");
    }
}
