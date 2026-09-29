//! Matcher result publisher: publish the result, then acknowledge the job.
//!
//! Results go to the requesting owner's reply inbox over transient Core NATS.
//! The raw event remains unacknowledged until the orchestrator commits the
//! result, so a lost result is recomputed from that raw event.

use core::str::FromStr;
use core::time::Duration;

use async_nats::HeaderValue;
use thiserror::Error;
use tokio::time::Instant;

use routers_network::Entry;

use crate::bus::Wire;
use crate::bus::adapter::{AckHandle, PublishError, PublishOutcome, Publisher};
use crate::bus::outbound;
use crate::orchestrator::dispatch::STICKY_HEADER;
use crate::protocol::ids::headers::stamp_schema;
use crate::protocol::result::SolveResult;

/// The ceiling a doubling backoff may reach between retries.
const BACKOFF_CAP: Duration = Duration::from_secs(2);

/// How the publisher retries an ambiguous result publish before giving up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishConfig {
    /// The maximum publish attempts before [`PublishFailure::Exhausted`]. Counts
    /// the first attempt, so `1` means "try once, never retry".
    pub attempts: u32,
    /// The first inter-attempt pause. Doubles after each ambiguous attempt,
    /// capped at a fixed two-second maximum.
    pub backoff: Duration,
    /// The redelivery hint attached to the job's `nak`: how long the broker holds
    /// the job back before offering it again.
    pub nak_delay: Duration,
}

impl Default for PublishConfig {
    fn default() -> Self {
        Self {
            attempts: 5,
            backoff: Duration::from_millis(100),
            nak_delay: Duration::from_secs(5),
        }
    }
}

/// The result publisher accepted the result and acknowledged its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Published {
    /// How many publish attempts it took (`1` when the first landed).
    pub attempts: u32,
    /// `true` when a durable publisher recognised a stored duplicate.
    pub duplicate: bool,
    /// Encoded result size on the wire.
    pub bytes: usize,
    /// Time spent encoding the result once.
    pub encoded_for: Duration,
    /// Time spent publishing encoded bytes, including ambiguous retries.
    pub stored_for: Duration,
    /// Time spent acknowledging the source job after confirmed publication.
    pub acked_for: Duration,
}

/// Why a result could not be published-then-acked. In every case the job is left
/// for redelivery, so the work is retried and its msg-id deduplicates any copy.
#[derive(Debug, Error)]
pub enum PublishFailure {
    /// Every attempt returned [`PublishError::Ambiguous`]; a copy may or may not
    /// be stored. The job was `nak`ed for redelivery.
    #[error("result publish exhausted after {attempts} ambiguous attempt(s)")]
    Exhausted {
        /// The number of attempts made (equals [`PublishConfig::attempts`]).
        attempts: u32,
    },
    /// A publish returned [`PublishError::Failed`], or the result could not be
    /// encoded: nothing was stored. The job was `nak`ed for redelivery.
    #[error("result publish failed")]
    Failed(#[source] anyhow::Error),
    /// The result was stored, but acknowledging the job afterwards failed. The
    /// job is deliberately not `nak`ed — the broker redelivers it on `ack_wait`
    /// and the duplicate result deduplicates on its msg-id.
    #[error("result published but the job acknowledgement failed")]
    AckFailed(#[source] anyhow::Error),
}

/// Publishes solve results and acknowledges the jobs that produced them, in
/// that order. Production uses Core NATS with raw-event ownership; tests also
/// exercise the durable adapter against an in-memory bus.
#[derive(Clone, Debug)]
pub struct ResultPublisher<P> {
    publisher: P,
    cfg: PublishConfig,
    sticky: Option<HeaderValue>,
}

impl<P> ResultPublisher<P> {
    /// Wrap `publisher` with the retry policy `cfg`.
    #[must_use]
    pub fn new(publisher: P, cfg: PublishConfig) -> Self {
        Self {
            publisher,
            cfg,
            sticky: None,
        }
    }

    /// Advertise `subject` on every result as this matcher's direct request
    /// subject, so the owner can send the vehicle's next request here.
    ///
    /// # Errors
    ///
    /// Fails when `subject` is not a valid header value.
    pub fn with_sticky_subject(mut self, subject: &str) -> anyhow::Result<Self> {
        self.sticky = Some(HeaderValue::from_str(subject)?);
        Ok(self)
    }

    /// Publish `result`, then acknowledge `job` — and only in that order.
    ///
    /// The result is encoded once and published to `subject`, the owner's
    /// reply inbox, under `Nats-Msg-Id = result.msg_id()` (the job id). The job is never
    /// acknowledged unless the broker acknowledged the result: an ambiguous send
    /// retries the identical bytes up to [`PublishConfig::attempts`] then
    /// [`PublishFailure::Exhausted`], a failed send `nak`s, and an ack that fails
    /// after a confirmed store returns [`PublishFailure::AckFailed`] without a `nak`.
    pub async fn publish_then_ack_to<E, H>(
        &self,
        result: &SolveResult<E>,
        job: H,
        subject: &str,
    ) -> Result<Published, PublishFailure>
    where
        E: Entry + serde::de::DeserializeOwned,
        H: AckHandle,
        P: Publisher<SolveResult<E>>,
    {
        let msg_id = result.msg_id();

        let mut headers = outbound();
        stamp_schema(&mut headers);
        if let Some(sticky) = &self.sticky {
            headers.insert(STICKY_HEADER, sticky.clone());
        }

        // Encode once so every retry republishes these exact bytes; an encode
        // failure lands nothing and is handled like a clean `Failed` publish.
        let encode_started = Instant::now();
        let bytes = match result.encode() {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = job.nak(Some(self.cfg.nak_delay)).await;
                return Err(PublishFailure::Failed(error));
            }
        };
        let encoded_for = encode_started.elapsed();

        let mut backoff = self.cfg.backoff;
        let mut attempt: u32 = 0;
        let store_started = Instant::now();
        loop {
            attempt += 1;
            match self
                .publisher
                .publish_bytes(subject, &msg_id, headers.clone(), &bytes)
                .await
            {
                Ok(PublishOutcome::Acked { duplicate, .. }) => {
                    let stored_for = store_started.elapsed();
                    // A durable adapter stored the result; a Core adapter only
                    // enqueued it, leaving recovery to the owned raw event.
                    let ack_started = Instant::now();
                    return match job.ack().await {
                        Ok(()) => Ok(Published {
                            attempts: attempt,
                            duplicate,
                            bytes: bytes.len(),
                            encoded_for,
                            stored_for,
                            acked_for: ack_started.elapsed(),
                        }),
                        Err(error) => Err(PublishFailure::AckFailed(error)),
                    };
                }
                Err(PublishError::Ambiguous(_)) if attempt < self.cfg.attempts => {
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2).min(BACKOFF_CAP);
                }
                Err(PublishError::Ambiguous(_)) => {
                    let _ = job.nak(Some(self.cfg.nak_delay)).await;
                    return Err(PublishFailure::Exhausted { attempts: attempt });
                }
                Err(PublishError::Failed(error)) => {
                    let _ = job.nak(Some(self.cfg.nak_delay)).await;
                    return Err(PublishFailure::Failed(error));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PublishConfig, PublishFailure, ResultPublisher};

    use alloc::sync::Arc;
    use core::time::Duration;
    use std::sync::Mutex;

    use async_nats::HeaderMap;
    use routers_network::mock::MockEntryId;
    use routers_transition::matcher::Continuation;

    use crate::bus::adapter::{AckHandle, PublishError, Publisher, Source};
    use crate::bus::memory::{MemoryAck, MemoryBus};
    use crate::event::VehicleId;
    use crate::protocol::ids::{GraphVersion, Lane, ObservationId, RegionId, SCHEMA_VERSION};
    use crate::protocol::job::{JobIdentity, SolveJob};
    use crate::protocol::result::{SolveOutcome, SolveResult};

    /// The subject the stand-in job message lives on, sharing no dedup group with
    /// the result plane so acking/naking it never touches result state.
    const JOB_SUBJECT: &str = "solve.jobs.test";

    /// The owner inbox the tests answer to, and its wildcard.
    const REPLY: &str = "_INBOX.test.p.3";
    const RESULTS: &str = "_INBOX.test.p.>";

    /// A representative result for `vehicle`, whose echoed id matches its identity.
    fn sample_result(vehicle: u64) -> SolveResult<MockEntryId> {
        let identity = JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(vehicle),
            observation: ObservationId {
                partition: 3,
                sequence: 99,
            },
            base: None,
            graph: GraphVersion::new("europe-2026-09").unwrap(),
            region: RegionId::new("paris").unwrap(),
        };
        let job = SolveJob::new(
            identity,
            Lane::DEFAULT,
            1_726_000_000_000_000,
            Continuation::Restart { fresh: Vec::new() },
        );
        SolveResult::new(&job, SolveOutcome::Unanchored, 1_726_000_000_500_000)
    }

    /// Fast timings so retrying tests do not sleep for real.
    fn fast_config() -> PublishConfig {
        PublishConfig {
            attempts: 5,
            backoff: Duration::from_millis(1),
            nak_delay: Duration::from_millis(50),
        }
    }

    /// Deliver a stand-in job message and hand back its ack handle.
    async fn job_handle(bus: &MemoryBus) -> MemoryAck {
        let publisher = bus.publisher::<SolveResult<MockEntryId>>();
        let mut source = bus.source::<SolveResult<MockEntryId>>(JOB_SUBJECT);
        publisher
            .publish(JOB_SUBJECT, "job-1", HeaderMap::new(), &sample_result(1))
            .await
            .expect("publish stand-in job");
        source
            .next()
            .await
            .expect("a delivery")
            .expect("it decodes")
            .handle
    }

    /// An ack handle whose acknowledgement always fails, recording its `nak`s so
    /// a test can assert one was (not) sent.
    struct FailingAck {
        naks: Arc<Mutex<Vec<Option<Duration>>>>,
    }

    impl AckHandle for FailingAck {
        async fn ack(self) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("ack rejected"))
        }

        async fn nak(self, delay: Option<Duration>) -> anyhow::Result<()> {
            self.naks.lock().unwrap().push(delay);
            Ok(())
        }

        fn sequence(&self) -> u64 {
            0
        }

        fn deliveries(&self) -> u32 {
            1
        }
    }

    #[tokio::test]
    async fn success_publishes_result_and_acks_job_once() {
        let bus = MemoryBus::new();
        let handle = job_handle(&bus).await;
        let result = sample_result(0x1234_5678);
        let publisher =
            ResultPublisher::new(bus.publisher::<SolveResult<MockEntryId>>(), fast_config());

        let published = publisher
            .publish_then_ack_to(&result, handle, REPLY)
            .await
            .expect("first publish lands");
        assert_eq!(published.attempts, 1);
        assert!(!published.duplicate);
        assert!(published.bytes > 0);

        let stored = bus.published(RESULTS);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].1.as_deref(), Some(result.msg_id().as_str()));
        assert_eq!(stored[0].0, REPLY);

        assert_eq!(bus.acked_count(JOB_SUBJECT), 1);
        assert!(bus.nak_delays().is_empty());
    }

    #[tokio::test]
    async fn ambiguous_then_success_dedups_and_acks() {
        let bus = MemoryBus::new();
        let handle = job_handle(&bus).await;
        let result = sample_result(0xdead_beef);
        let publisher =
            ResultPublisher::new(bus.publisher::<SolveResult<MockEntryId>>(), fast_config());

        // The first publish is ambiguous: the bus stores it, then the retry
        // deduplicates against that stored copy.
        bus.fail_next_publish(PublishError::Ambiguous(anyhow::anyhow!("ack lost")));

        let published = publisher
            .publish_then_ack_to(&result, handle, REPLY)
            .await
            .expect("retry lands");
        assert_eq!(published.attempts, 2);
        assert!(
            published.duplicate,
            "the retry deduplicated against the stored copy"
        );

        assert_eq!(bus.published(RESULTS).len(), 1);
        assert_eq!(bus.acked_count(JOB_SUBJECT), 1);
        assert!(bus.nak_delays().is_empty());
    }

    #[tokio::test]
    async fn failed_publish_naks_job_without_acking() {
        let bus = MemoryBus::new();
        let handle = job_handle(&bus).await;
        let result = sample_result(7);
        let cfg = fast_config();
        let nak_delay = cfg.nak_delay;
        let publisher = ResultPublisher::new(bus.publisher::<SolveResult<MockEntryId>>(), cfg);

        bus.fail_next_publish(PublishError::Failed(anyhow::anyhow!("refused")));

        let failure = publisher
            .publish_then_ack_to(&result, handle, REPLY)
            .await
            .expect_err("a failed publish surfaces");
        assert!(
            matches!(failure, PublishFailure::Failed(_)),
            "got {failure:?}"
        );

        assert!(bus.published(RESULTS).is_empty());
        assert_eq!(bus.acked_count(JOB_SUBJECT), 0);
        assert_eq!(bus.nak_delays(), vec![Some(nak_delay)]);
    }

    #[tokio::test]
    async fn exhausted_after_one_ambiguous_attempt_naks() {
        let bus = MemoryBus::new();
        let handle = job_handle(&bus).await;
        let result = sample_result(99);
        // A one-shot bus knob plus a one-attempt budget exhausts deterministically.
        let cfg = PublishConfig {
            attempts: 1,
            backoff: Duration::from_millis(1),
            nak_delay: Duration::from_millis(50),
        };
        let nak_delay = cfg.nak_delay;
        let publisher = ResultPublisher::new(bus.publisher::<SolveResult<MockEntryId>>(), cfg);

        bus.fail_next_publish(PublishError::Ambiguous(anyhow::anyhow!("timeout")));

        let failure = publisher
            .publish_then_ack_to(&result, handle, REPLY)
            .await
            .expect_err("the single attempt exhausts");
        match failure {
            PublishFailure::Exhausted { attempts } => assert_eq!(attempts, 1),
            other => panic!("expected Exhausted, got {other:?}"),
        }

        // The ambiguous send stored a copy, but the job is naked, not acked.
        assert_eq!(bus.acked_count(JOB_SUBJECT), 0);
        assert_eq!(bus.nak_delays(), vec![Some(nak_delay)]);
    }

    #[tokio::test]
    async fn ack_failure_after_publish_reports_ack_failed_without_naking() {
        let bus = MemoryBus::new();
        let naks = Arc::new(Mutex::new(Vec::new()));
        let handle = FailingAck { naks: naks.clone() };
        let result = sample_result(3);
        let publisher =
            ResultPublisher::new(bus.publisher::<SolveResult<MockEntryId>>(), fast_config());

        let failure = publisher
            .publish_then_ack_to(&result, handle, REPLY)
            .await
            .expect_err("the job ack fails");
        assert!(
            matches!(failure, PublishFailure::AckFailed(_)),
            "got {failure:?}"
        );

        // The result landed and the job was not naked: redelivery plus dedup covers it.
        assert_eq!(bus.published(RESULTS).len(), 1);
        assert!(naks.lock().unwrap().is_empty());
    }
}
