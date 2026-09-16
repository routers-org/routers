//! Crash boundaries and graceful shutdown — the durability contract.
//!
//! A commit is prepare → publish → promote; a crash at any step leaves one
//! recoverable state a restart resolves without re-emitting. Two boundaries stage
//! the durable residue directly through the committer (the in-memory bus cannot
//! resume a consumer past a committed-but-unacked raw); the after-publish
//! boundary uses a real worker and [`crash`].
//! [`crash`]: super::fixture::OrchestratorHandle::crash

use super::fixture::*;

/// A crash after publish but before promote: recovery re-publishes (deduplicated)
/// and promotes, so exactly one output survives and the checkpoint lands.
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

    // Set the frontier the crash never persisted, so the redelivered raw is suppressed, not re-dispatched.
    fleet
        .store
        .set_frontier(routers_realtime::store::checkpoint::PartitionFrontier {
            partition,
            sequence: observation.sequence,
        })
        .await
        .unwrap();

    let restarted = fleet.spawn_orchestrator(partition);
    fleet.settle().await;
    let _ = restarted.stop().await;
    matcher.stop().await;

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

/// A crash before publish leaves an unpublished prepared record; recovery
/// publishes then promotes it.
#[tokio::test(start_paused = true)]
async fn crash_before_publish_recovers_and_publishes() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

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

/// A graceful stop with a job in flight leaves the raw unacked and no prepared
/// record, so a restart replays the raw and completes it.
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
