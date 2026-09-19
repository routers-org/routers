//! Deadlines, the deadline/commit race, contiguous frontier advance, and
//! admission backpressure — the §8 scheduling rules and their §10 rows.

use core::time::Duration;

use super::fixture::*;

/// The concrete solve-jobs plane filter.
fn jobs_filter() -> &'static str {
    "solve.v1.g.>"
}

/// A never-answered job runs out its freshness budget and commits a `Terminal`;
/// a straggling late answer is then rejected, not committed twice.
#[tokio::test(start_paused = true)]
async fn deadline_expiry_commits_terminal_and_rejects_late_result() {
    // A brisk 100 ms budget so the deadline fires quickly under paused time.
    let fleet = Fleet::bent_road_with_budget(100);
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    let matcher = fleet.spawn_matcher(MatcherBehaviour::silent());
    let orchestrator = fleet.spawn_orchestrator(partition);
    let observation = fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;

    fleet.settle().await;
    assert_eq!(
        published_outputs(&fleet.bus, partition).len(),
        1,
        "the deadline committed one terminal",
    );

    let job = published_jobs(&fleet.bus)
        .into_iter()
        .find(|job| job.identity.observation == observation)
        .expect("expired job was published");
    fleet.publish_result(&job, SolveOutcome::Unanchored).await;
    fleet.settle().await;

    let stats = orchestrator.stop().await;
    matcher.stop().await;

    assert_eq!(stats.terminal, 1, "one terminal was committed");
    assert_eq!(stats.committed, 1, "and only one commit overall");
    assert_eq!(stats.rejected, 1, "the late answer was rejected");
    assert_eq!(
        published_outputs(&fleet.bus, partition).len(),
        1,
        "no second output for the late answer",
    );
    let output = &published_outputs(&fleet.bus, partition)[0];
    assert!(
        matches!(
            output.kind,
            OutputKind::Terminal {
                reason: TerminalReason::DeadlineExpired,
                ..
            }
        ),
        "the terminal is tagged deadline-expired, got {}",
        output.kind.kind(),
    );
}

/// An answer landing just before the deadline wins: the commit is a match, no terminal.
#[tokio::test(start_paused = true)]
async fn prepared_commit_beats_deadline() {
    // A 40 ms answer under a 100 ms budget: the result commits first.
    let fleet = Fleet::bent_road_with_budget(100);
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    let matcher = fleet.spawn_matcher(MatcherBehaviour::delayed_layer(Duration::from_millis(40)));
    let orchestrator = fleet.spawn_orchestrator(partition);
    fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;

    fleet.settle().await;
    let stats = orchestrator.stop().await;
    matcher.stop().await;

    assert_eq!(stats.terminal, 0, "the answer beat the deadline");
    assert_eq!(stats.committed, 1, "exactly one commit");
    let outputs = published_outputs(&fleet.bus, partition);
    assert_eq!(outputs.len(), 1, "one output");
    assert!(
        matches!(outputs[0].kind, OutputKind::Matched { .. }),
        "the commit is a match, not a terminal",
    );
}

/// Two vehicles share a partition; the lower-sequence one's answer lags. The
/// frontier holds below its sequence until it completes, then covers both.
#[tokio::test(start_paused = true)]
async fn out_of_order_completion_advances_frontier_contiguously() {
    let a = 1u64;
    let b = same_partition_as(a);
    let fleet = Fleet::bent_road();
    let partition = fleet.partition_of(a);
    assert_eq!(
        fleet.partition_of(b),
        partition,
        "the two share a partition"
    );

    let (materializer, _sink) = fleet.spawn_materializer();
    // A's answer lags 60 ms; B's is immediate.
    let matcher = fleet.spawn_matcher_with_slow(a, Duration::from_millis(60));
    let orchestrator = fleet.spawn_orchestrator(partition);

    // A ingested first, so A's completion gates the frontier over B's.
    let a_obs = fleet.ingest(a, obs_ts(0), road_points()[0]).await;
    let b_obs = fleet.ingest(b, obs_ts(0), road_points()[0]).await;
    assert!(
        a_obs.sequence < b_obs.sequence,
        "A holds the lower sequence"
    );

    advance_until(|| !published_outputs(&fleet.bus, partition).is_empty()).await;
    let held = fleet.store.frontier(partition).await.unwrap();
    assert!(
        held.map_or(0, |f| f.sequence) < a_obs.sequence,
        "the frontier holds below A's sequence while A lags (was {held:?})",
    );

    fleet.settle().await;
    let stats = orchestrator.stop().await;
    matcher.stop().await;
    let _ = materializer.stop().await;

    assert_eq!(stats.committed, 2, "both vehicles committed");
    assert_eq!(
        stats.frontier, b_obs.sequence,
        "the frontier jumped to cover both once A completed",
    );
    assert!(
        fleet.store.load(VehicleId(a)).await.unwrap().0.is_some(),
        "A has a checkpoint",
    );
    assert!(
        fleet.store.load(VehicleId(b)).await.unwrap().0.is_some(),
        "B has a checkpoint",
    );
}

/// With a per-region ceiling of one outstanding job, two vehicles sharing a
/// partition cannot both have a job on the plane: the second is held until the first finishes.
#[tokio::test(start_paused = true)]
async fn admission_caps_hold_work_outside_matcher_queues() {
    let a = 1u64;
    let b = same_partition_as(a);
    let fleet = Fleet::bent_road().with_admission(AdmissionConfig {
        region_jobs: 1,
        ..AdmissionConfig::default()
    });
    let partition = fleet.partition_of(a);

    // A 20 ms answer holds the single slot long enough to observe the second held.
    let matcher = fleet.spawn_matcher(MatcherBehaviour::delayed_layer(Duration::from_millis(20)));
    let orchestrator = fleet.spawn_orchestrator(partition);
    fleet.ingest(a, obs_ts(0), road_points()[0]).await;
    fleet.ingest(b, obs_ts(0), road_points()[0]).await;

    advance_until(|| !fleet.bus.published(jobs_filter()).is_empty()).await;
    assert_eq!(
        fleet.bus.published(jobs_filter()).len(),
        1,
        "the second vehicle's job is held outside the queue",
    );

    fleet.settle().await;
    let stats = orchestrator.stop().await;
    matcher.stop().await;

    assert!(stats.held >= 1, "the second vehicle was held by admission");
    assert_eq!(stats.committed, 2, "both vehicles completed");
    assert_eq!(
        fleet.bus.published(jobs_filter()).len(),
        2,
        "both jobs eventually dispatched, one at a time",
    );
    assert_eq!(
        published_outputs(&fleet.bus, partition).len(),
        2,
        "two outputs, one per vehicle",
    );
}

/// A full per-vehicle raw FIFO returns ownership to the broker rather than
/// acknowledging and losing the valid observation. Once the head completes,
/// redelivery enters the FIFO and commits normally.
#[tokio::test(start_paused = true)]
async fn pending_overflow_is_redelivered_after_capacity_returns() {
    let fleet = Fleet::bent_road().with_pending_limit(1);
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    let orchestrator = fleet.spawn_orchestrator(partition);

    let first = fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;
    advance_until(|| !fleet.bus.published(jobs_filter()).is_empty()).await;
    let second = fleet.ingest(vehicle, obs_ts(1), road_points()[1]).await;
    advance_until(|| !fleet.bus.nak_delays().is_empty()).await;

    assert!(second.sequence > first.sequence);
    assert_eq!(
        fleet
            .bus
            .acked_count(&routers_realtime::topology::raw_subject(u64::from(
                partition
            ))),
        0,
        "neither the active head nor the backpressured raw is acknowledged",
    );

    let matcher = fleet.spawn_matcher(MatcherBehaviour::scripted_layer());
    fleet.settle().await;
    let stats = orchestrator.stop().await;
    matcher.stop().await;

    assert!(stats.deferred >= 1);
    assert_eq!(stats.committed, 2);
    assert_eq!(stats.frontier, second.sequence);
    assert_eq!(
        fleet
            .bus
            .acked_count(&routers_realtime::topology::raw_subject(u64::from(
                partition
            ))),
        2,
    );
}
