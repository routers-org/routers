//! Regional solve-job plane.
//!
//! Jobs are addressed by `(graph version, region, priority lane)` on the
//! subject `solve.v1.g.<graph>.r.<region>.q.<lane>`. One
//! [`WorkQueue`](async_nats::jetstream::stream::RetentionPolicy::WorkQueue)
//! stream per region claims each job once; replicas serving a `(graph, region)`
//! share a durable pull consumer whose filter narrows the region stream to that
//! graph, so the broker load-balances without any two solving the same job.

use core::time::Duration;

use anyhow::Context as _;
use async_nats::jetstream::{
    self,
    consumer::{AckPolicy, PullConsumer, pull},
    stream::{Config, RetentionPolicy, StorageType},
};

use super::{create_or_update_stream, duplicate_window};
use crate::protocol::ids::{GraphVersion, Lane, RegionId};

/// The versioned subject prefix every solve job shares.
pub const JOB_PREFIX: &str = "solve.v1";

/// Retention and delivery knobs for the regional job plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobsConfig {
    /// How long an unacknowledged job lives before it ages out. Zero means it
    /// is retained until acknowledged.
    pub max_age: Duration,
    /// How many times a job is redelivered before the broker gives up. A
    /// negative value means unlimited redelivery.
    pub max_deliver: i64,
    /// How long the broker waits for an ack before redelivering.
    pub ack_wait: Duration,
    /// Unclaimed-but-delivered jobs the consumer may hold across all replicas.
    pub max_ack_pending: i64,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            // Freshness is measured at the matcher; it is not a retention
            // policy. Retain an unanswered job until it is acked so replayed
            // backlog cannot silently strand its raw observation active.
            max_age: Duration::ZERO,
            // Bounded concurrent handler/CPU capacity lives in PullConfig.
            // Delivery itself is retried until a matcher publishes-and-acks a
            // result, rather than disappearing after an arbitrary wall time.
            max_deliver: -1,
            ack_wait: Duration::from_secs(30),
            // This durable is shared by all replicas in a region. Sixty-four
            // covers the catalog's default eight replicas at eight slots each
            // without allowing a stalled fleet to preclaim thousands of jobs.
            max_ack_pending: 64,
        }
    }
}

/// The subject one job publishes to: `solve.v1.g.<graph>.r.<region>.q.<lane>`.
pub fn job_subject(graph: &GraphVersion, region: &RegionId, lane: Lane) -> String {
    format!("{JOB_PREFIX}.g.{graph}.r.{region}.q.{}", lane.0)
}

/// The name of a region's job stream: `SOLVE-JOBS-<region>`.
pub fn job_stream_name(region: &RegionId) -> String {
    format!("SOLVE-JOBS-{region}")
}

/// The subject set a region's stream captures: every graph and lane for the
/// region, `solve.v1.g.*.r.<region>.q.*`.
pub fn job_stream_subjects(region: &RegionId) -> String {
    format!("{JOB_PREFIX}.g.*.r.{region}.q.*")
}

/// The durable consumer name matchers serving one `(graph, region)` share:
/// `matchers-g<graph>-r<region>`.
pub fn job_consumer_name(graph: &GraphVersion, region: &RegionId) -> String {
    format!("matchers-g{graph}-r{region}")
}

/// The filter narrowing a region stream to one graph, across every lane:
/// `solve.v1.g.<graph>.r.<region>.q.>`.
pub fn job_consumer_filter(graph: &GraphVersion, region: &RegionId) -> String {
    format!("{JOB_PREFIX}.g.{graph}.r.{region}.q.>")
}

/// Recover the `(graph, region, lane)` a job subject addressed, validating each
/// token. Returns [`None`] for anything not a well-formed job subject.
pub fn parse_job_subject(subject: &str) -> Option<(GraphVersion, RegionId, Lane)> {
    let parts: [&str; 8] = subject.split('.').collect::<Vec<_>>().try_into().ok()?;
    match parts {
        ["solve", "v1", "g", graph, "r", region, "q", lane] => {
            let graph = GraphVersion::new(graph).ok()?;
            let region = RegionId::new(region).ok()?;
            let lane = lane.parse::<u8>().ok().map(Lane)?;
            Some((graph, region, lane))
        }
        _ => None,
    }
}

/// The stream every process derives identically for a region.
fn job_stream_config(region: &RegionId, config: &JobsConfig) -> Config {
    Config {
        name: job_stream_name(region),
        subjects: vec![job_stream_subjects(region)],
        retention: RetentionPolicy::WorkQueue,
        storage: StorageType::File,
        max_age: config.max_age,
        duplicate_window: duplicate_window(config.max_age),
        ..Default::default()
    }
}

/// Idempotently reconcile a region's job stream.
pub async fn ensure_job_stream(
    context: &jetstream::Context,
    region: &RegionId,
    config: &JobsConfig,
) -> anyhow::Result<jetstream::stream::Stream> {
    create_or_update_stream(context, job_stream_config(region, config))
        .await
        .with_context(|| format!("could not reconcile job stream for region {region}"))
}

/// Open a region's job stream, creating it from `config` only if it is missing.
/// Matchers use this so the orchestrator's tunables stay the stream's configuration.
pub async fn open_job_stream(
    context: &jetstream::Context,
    region: &RegionId,
    config: &JobsConfig,
) -> anyhow::Result<jetstream::stream::Stream> {
    context
        .get_or_create_stream(job_stream_config(region, config))
        .await
        .with_context(|| format!("could not open job stream for region {region}"))
}

/// The shared durable pull consumer for the matchers of one `(graph, region)`.
pub async fn job_consumer(
    stream: &jetstream::stream::Stream,
    graph: &GraphVersion,
    region: &RegionId,
    config: &JobsConfig,
) -> anyhow::Result<PullConsumer> {
    let name = job_consumer_name(graph, region);
    let desired = pull::Config {
        durable_name: Some(name.clone()),
        filter_subject: job_consumer_filter(graph, region),
        ack_policy: AckPolicy::Explicit,
        max_deliver: config.max_deliver,
        ack_wait: config.ack_wait,
        max_ack_pending: config.max_ack_pending,
        ..Default::default()
    };

    let consumer = stream
        .get_or_create_consumer(&name, desired.clone())
        .await
        .with_context(|| format!("could not create job consumer {name}"))?;

    if consumer.cached_info().config.max_ack_pending == config.max_ack_pending {
        return Ok(consumer);
    }

    stream
        .update_consumer(desired)
        .await
        .with_context(|| format!("could not resize job consumer {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> GraphVersion {
        GraphVersion::new("europe-2026-09").unwrap()
    }

    fn region() -> RegionId {
        RegionId::new("syd").unwrap()
    }

    #[test]
    fn subject_and_names_format() {
        assert_eq!(
            job_subject(&graph(), &region(), Lane(0)),
            "solve.v1.g.europe-2026-09.r.syd.q.0"
        );
        assert_eq!(
            job_subject(&graph(), &region(), Lane(7)),
            "solve.v1.g.europe-2026-09.r.syd.q.7"
        );
        assert_eq!(job_stream_name(&region()), "SOLVE-JOBS-syd");
        assert_eq!(job_stream_subjects(&region()), "solve.v1.g.*.r.syd.q.*");
        assert_eq!(
            job_consumer_name(&graph(), &region()),
            "matchers-geurope-2026-09-rsyd"
        );
        assert_eq!(
            job_consumer_filter(&graph(), &region()),
            "solve.v1.g.europe-2026-09.r.syd.q.>"
        );
    }

    #[test]
    fn defaults_retain_unanswered_work_for_redelivery() {
        let cfg = JobsConfig::default();
        assert_eq!(cfg.max_age, Duration::ZERO);
        assert!(cfg.max_deliver < 0, "negative means unlimited delivery");
        assert!(cfg.max_ack_pending > 0, "claiming remains capacity bounded");
    }

    #[test]
    fn subject_round_trips_through_parser() {
        for lane in [0u8, 1, 9, 255] {
            let subject = job_subject(&graph(), &region(), Lane(lane));
            assert_eq!(
                parse_job_subject(&subject),
                Some((graph(), region(), Lane(lane)))
            );
        }
    }

    #[test]
    fn parser_rejects_the_malformed() {
        let rejected = [
            "events.raw.p.3",                    // foreign plane
            "solve.v1.g.europe.r.syd.q.>",       // wildcard, not a concrete lane
            "solve.v1.g.europe.r.syd.q.-1",      // signed lane
            "solve.v1.g.europe.r.syd.q.256",     // lane overflows u8
            "solve.v1.g.europe.r.syd.q",         // too few tokens
            "solve.v1.g.europe.r.syd.q.0.extra", // too many tokens
            "solve.v2.g.europe.r.syd.q.0",       // wrong version
            "solve.v1.x.europe.r.syd.q.0",       // wrong graph marker
            "",                                  // empty
        ];
        for subject in rejected {
            assert_eq!(
                parse_job_subject(subject),
                None,
                "{subject:?} should not parse"
            );
        }
    }

    #[test]
    fn default_delivery_window_matches_the_regional_capacity_envelope() {
        assert_eq!(JobsConfig::default().max_ack_pending, 64);
    }
}
