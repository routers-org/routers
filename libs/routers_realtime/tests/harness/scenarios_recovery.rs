//! Crash boundaries and graceful shutdown — the durability contract. (T31)
//!
//! Spec §9/§10: a commit is prepare → publish → promote, and a crash at any step
//! leaves exactly one recoverable state that a restart resolves without
//! re-emitting or diverging a committed history. These scenarios reach each crash
//! boundary and then respawn the orchestrator, whose recovery pass must finish
//! the commit idempotently.
//!
//! Two of the boundaries are reached by *staging* the durable residue directly
//! through the committer, the same technique the merged recovery unit tests use:
//! the in-memory bus cannot reposition a consumer past a committed-but-unacked
//! raw the way a JetStream durable resumes at `frontier + 1`, so redelivering
//! that raw into a fresh worker would (correctly) trip the worker's own
//! timestamp-regression assertion. Staging the exact store state a crash would
//! leave keeps the recovery path under test without that artefact. The
//! after-publish boundary is reached with a real worker and [`crash`] to exercise
//! the crash primitive end to end.
//!
//! [`crash`]: super::fixture::OrchestratorHandle::crash

use super::fixture::*;

/// A crash *after* the output is published but *before* the checkpoint is
/// promoted: recovery re-publishes (the broker deduplicates) and promotes, so
/// exactly one output survives and the checkpoint lands.
#[tokio::test(start_paused = true)]
async fn crash_after_publish_before_promote_recovers_once() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    // Arm a one-shot promote failure so the first commit blocks post-publish.
    fleet.store.fail_next(Op::Promote);
    let matcher = fleet.spawn_matcher(MatcherBehaviour::scripted_layer());
    let orchestrator = fleet.spawn_orchestrator(partition);
    let observation = fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;

    // Catch the worker in the blocked, post-publish state: the output is durable
    // and a prepared record survives, before the 20 ms retry would finish it.
    advance_until(|| {
        !published_outputs(&fleet.bus, partition).is_empty()
            && fleet
                .store
                .snapshot()
                .prepared
                .contains_key(&VehicleId(vehicle))
    })
    .await;
    orchestrator.crash();

    // The crashed worker never advanced or persisted the frontier; the output for
    // this sequence is durable, so reflect that before restarting, so the
    // redelivered raw is suppressed as behind-frontier rather than re-dispatched.
    fleet
        .store
        .set_frontier(routers_realtime::store::checkpoint::PartitionFrontier {
            partition,
            sequence: observation.sequence,
        })
        .await
        .unwrap();

    // Restart: recovery finishes the prepared commit.
    let restarted = fleet.spawn_orchestrator(partition);
    fleet.settle().await;
    let _ = restarted.stop().await;
    matcher.stop().await;

    // Exactly one output survived (the republish deduplicated), and the
    // checkpoint was promoted.
    assert_eq!(
        published_outputs(&fleet.bus, partition).len(),
        1,
        "the output was not duplicated by recovery",
    );
    let (checkpoint, prepared) = fleet.store.load(VehicleId(vehicle)).await.unwrap();
    assert_eq!(
        checkpoint.expect("checkpoint promoted").revision,
        Revision(observation.sequence),
    );
    assert!(prepared.is_none(), "the prepared record was promoted away");
}

/// A crash *before* the output is published leaves an unpublished prepared
/// record; recovery publishes then promotes it.
#[tokio::test(start_paused = true)]
async fn crash_before_publish_recovers_and_publishes() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    // Stage the durable residue of a crash between prepare and publish: a
    // prepared record whose output never landed.
    let observation = fleet.stage_unpublished(vehicle, partition, 1).await;
    assert!(
        published_outputs(&fleet.bus, partition).is_empty(),
        "nothing was published before the crash",
    );
    assert!(
        fleet
            .store
            .snapshot()
            .prepared
            .contains_key(&VehicleId(vehicle)),
        "the prepared record survived the crash",
    );

    // Restart: recovery publishes then promotes the surviving record.
    let orchestrator = fleet.spawn_orchestrator(partition);
    fleet.settle().await;
    let _ = orchestrator.stop().await;

    assert_eq!(
        published_outputs(&fleet.bus, partition).len(),
        1,
        "recovery published the surviving output exactly once",
    );
    let (checkpoint, prepared) = fleet.store.load(VehicleId(vehicle)).await.unwrap();
    assert_eq!(
        checkpoint.expect("checkpoint promoted").revision,
        Revision(observation.sequence),
    );
    assert!(prepared.is_none(), "nothing is left staged");
}

/// A graceful stop while a job is in flight abandons the dispatch cleanly: the
/// raw is left unacked and no prepared record is written, so a restart replays
/// the raw and completes it.
#[tokio::test(start_paused = true)]
async fn shutdown_drains_and_leaves_recoverable_state() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    // No matcher: the job dispatches and stays in flight.
    let orchestrator = fleet.spawn_orchestrator(partition);
    fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;
    advance_until(|| !fleet.bus.published("solve.v1.g.>").is_empty()).await;

    let stats = orchestrator.stop().await;
    assert_eq!(stats.dispatched, 1, "the job was dispatched");
    assert_eq!(stats.committed, 0, "but never committed");
    assert!(
        fleet.store.snapshot().prepared.is_empty(),
        "an in-flight job leaves no prepared record",
    );
    assert_eq!(
        fleet
            .bus
            .acked_count(&routers_realtime::topology::raw_subject(u64::from(
                partition
            ))),
        0,
        "the raw is left unacked for replay",
    );

    // Restart with a matcher: the replayed raw completes.
    let matcher = fleet.spawn_matcher(MatcherBehaviour::scripted_layer());
    let restarted = fleet.spawn_orchestrator(partition);
    fleet.settle().await;
    let stats = restarted.stop().await;
    matcher.stop().await;

    assert_eq!(stats.committed, 1, "the restart completed the observation");
    assert_eq!(
        published_outputs(&fleet.bus, partition).len(),
        1,
        "exactly one output after recovery",
    );
    assert!(
        fleet
            .store
            .load(VehicleId(vehicle))
            .await
            .unwrap()
            .0
            .is_some(),
        "a checkpoint was committed",
    );
}
