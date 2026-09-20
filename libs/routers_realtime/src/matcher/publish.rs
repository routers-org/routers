//! Matcher result publisher: publish the result, then acknowledge the job.
//!
//! The load-bearing invariant: never acknowledge the job until the broker has
//! acknowledged the result — a crash before the result lands redelivers the
//! un-acked job (a duplicate solve is nominal). The result's `Nats-Msg-Id` is
//! the job id, so an [`PublishError::Ambiguous`] retry dedups; a
//! [`PublishError::Failed`] send stored nothing and the job is redelivered.

use core::time::Duration;

use thiserror::Error;

use routers_network::Entry;

use crate::bus::Wire;
use crate::bus::adapter::{AckHandle, PublishError, PublishOutcome, Publisher};
use crate::bus::outbound;
use crate::protocol::ids::headers::stamp_schema;
use crate::protocol::result::SolveResult;
use crate::topology::results;

/// The ceiling a doubling backoff may reach between retries.
const BACKOFF_CAP: Duration = Duration::from_secs(2);

/// How the publisher retries an ambiguous result publish before giving up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishConfig {
    /// The maximum publish attempts before [`PublishFailure::Exhausted`]. Counts
    /// the first attempt, so `1` means "try once, never retry".
    pub attempts: u32,
    /// The first inter-attempt pause. Doubles after each ambiguous attempt,
    /// capped at [`BACKOFF_CAP`].
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

/// A confirmed publish-then-ack: the broker stored (or deduplicated) the result
/// and the job was acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Published {
    /// How many publish attempts it took (`1` when the first landed).
    pub attempts: u32,
    /// `true` when the broker recognised the result as a duplicate it already
    /// held, rather than a fresh store.
    pub duplicate: bool,
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
/// that order. Generic over the [`Publisher`] `P` so production wires it to
/// JetStream while tests drive it against the in-memory bus.
#[derive(Clone, Debug)]
pub struct ResultPublisher<P> {
    publisher: P,
    cfg: PublishConfig,
}

impl<P> ResultPublisher<P> {
    /// Wrap `publisher` with the retry policy `cfg`.
    #[must_use]
    pub fn new(publisher: P, cfg: PublishConfig) -> Self {
        Self { publisher, cfg }
    }

    /// Publish `result`, then acknowledge `job` — and only in that order.
    ///
    /// The result is encoded once and published under
    /// `Nats-Msg-Id = result.msg_id()` (the job id). The job is never
    /// acknowledged unless the broker acknowledged the result: an ambiguous send
    /// retries the identical bytes up to [`PublishConfig::attempts`] then
    /// [`PublishFailure::Exhausted`], a failed send `nak`s, and an ack that fails
    /// after a confirmed store returns [`PublishFailure::AckFailed`] without a `nak`.
    pub async fn publish_then_ack<E, H>(
        &self,
        result: &SolveResult<E>,
        job: H,
    ) -> Result<Published, PublishFailure>
    where
        // `SolveResult<E>: Wire` holds only when `E` deserialises; `Entry` alone
        // guarantees only `Serialize`, so the extra bound is stated here.
        E: Entry + serde::de::DeserializeOwned,
        H: AckHandle,
        P: Publisher<SolveResult<E>>,
    {
        let subject = results::result_subject(u64::from(result.partition()));
        let msg_id = result.msg_id();

        let mut headers = outbound();
        stamp_schema(&mut headers);

        // Encode once so every retry republishes these exact bytes; an encode
        // failure lands nothing and is handled like a clean `Failed` publish.
        let bytes = match result.encode() {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = job.nak(Some(self.cfg.nak_delay)).await;
                return Err(PublishFailure::Failed(error));
            }
        };

        let mut backoff = self.cfg.backoff;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match self
                .publisher
                .publish_bytes(&subject, &msg_id, headers.clone(), &bytes)
                .await
            {
                Ok(PublishOutcome::Acked { duplicate, .. }) => {
                    // The broker has the result: only now may the job retire. A
                    // failed ack is not `nak`ed — redelivery plus dedup covers it.
                    return match job.ack().await {
                        Ok(()) => Ok(Published {
                            attempts: attempt,
                            duplicate,
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
    use super::{PublishConfig, PublishFailure, Published, ResultPublisher};

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
    use crate::topology::results;

    /// The subject the stand-in job message lives on, sharing no dedup group with
    /// the result plane so acking/naking it never touches result state.
    const JOB_SUBJECT: &str = "solve.jobs.test";

    /// The result-plane wildcard used to inspect what the publisher stored.
    const RESULTS: &str = "solve-result.v1.p.>";

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
        let job = SolveJob {
            id: identity.job_id(),
            identity,
            lane: Lane::DEFAULT,
            deadline_us: 1_726_000_000_000_000,
            context: Continuation::Restart { fresh: Vec::new() },
        };
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
            .publish_then_ack(&result, handle)
            .await
            .expect("first publish lands");
        assert_eq!(
            published,
            Published {
                attempts: 1,
                duplicate: false,
            }
        );

        let stored = bus.published(RESULTS);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].1.as_deref(), Some(result.msg_id().as_str()));
        assert_eq!(
            stored[0].0,
            results::result_subject(u64::from(result.partition()))
        );

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
            .publish_then_ack(&result, handle)
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
            .publish_then_ack(&result, handle)
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
            .publish_then_ack(&result, handle)
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
            .publish_then_ack(&result, handle)
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
