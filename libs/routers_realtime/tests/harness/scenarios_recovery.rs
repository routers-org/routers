//! Crash boundaries and graceful shutdown — the durability contract.
//!
//! A commit is prepare → publish → promote; a crash at any step leaves one
//! recoverable state a restart resolves without re-emitting. Two boundaries stage
//! the durable residue directly through the committer (the in-memory bus cannot
//! resume a consumer past a committed-but-unacked raw); the after-publish
//! boundary uses a real worker and [`crash`].
//! [`crash`]: super::fixture::OrchestratorHandle::crash

use super::fixture::*;
use core::time::Duration;

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

/// Direct mode keeps no prepared record: a crash after the output is durable
/// but before the checkpoint lands re-drives the still-unacked raw event, and
/// the deterministic re-solve republishes the same output id, which dedups.
#[tokio::test(start_paused = true)]
async fn direct_crash_after_publish_before_checkpoint_redrives_once() {
    let fleet = Fleet::bent_road().with_commit_mode(FleetCommitMode::Direct);
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    fleet.store.fail_next(Op::CommitDirect);
    let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
    let orchestrator = fleet.spawn_orchestrator(partition);
    let observation = fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;

    advance_until(|| !published_outputs(&fleet.bus, partition).is_empty()).await;
    orchestrator.crash();
    let first = published_outputs(&fleet.bus, partition);
    assert!(
        fleet.store.snapshot().prepared.is_empty(),
        "direct commits never stage"
    );
    assert!(
        fleet
            .store
            .load(VehicleId(vehicle))
            .await
            .unwrap()
            .0
            .is_none(),
        "the failed checkpoint write left no checkpoint"
    );

    fleet.redeliver_raw(partition);
    let restarted = fleet.spawn_orchestrator(partition);
    fleet.settle().await;
    let stats = restarted.stop().await;
    matcher.stop().await;

    assert_eq!(
        stats.committed, 1,
        "the restart committed the re-driven raw"
    );
    let outputs = published_outputs(&fleet.bus, partition);
    assert_eq!(outputs.len(), first.len(), "the re-solve deduplicated");
    assert_eq!(
        outputs.iter().map(|o| o.id).collect::<Vec<_>>(),
        first.iter().map(|o| o.id).collect::<Vec<_>>(),
        "the re-solve minted the same output ids",
    );
    let (checkpoint, prepared) = fleet.store.load(VehicleId(vehicle)).await.unwrap();
    assert_eq!(
        checkpoint.expect("checkpoint installed").revision,
        Revision(observation.sequence),
    );
    assert!(prepared.is_none());
}

/// A deferred owner that stops gracefully persists every vehicle it still
/// holds, so its frontier reaches the last raw event and nothing replays.
#[tokio::test(start_paused = true)]
async fn deferred_graceful_stop_persists_everything() {
    let fleet = Fleet::bent_road()
        .with_commit_mode(FleetCommitMode::Deferred)
        .with_checkpoint_policy(CheckpointPolicy {
            every: 1_000,
            interval: Duration::from_secs(3_600),
            in_flight: 8,
        });
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
    let orchestrator = fleet.spawn_orchestrator(partition);
    let mut last = None;
    for (i, &p) in road_points().iter().enumerate() {
        last = Some(fleet.ingest(vehicle, obs_ts(i), p).await);
    }
    let last = last.unwrap();
    fleet.settle().await;
    assert!(
        fleet
            .store
            .load(VehicleId(vehicle))
            .await
            .unwrap()
            .0
            .is_none(),
        "nothing is persisted before the policy makes it due"
    );
    let stats = orchestrator.stop().await;
    matcher.stop().await;

    assert_eq!(stats.committed, 6);
    assert_eq!(stats.persisted, 1, "one persist covers all six commits");
    assert_eq!(stats.frontier, last.sequence);
    let checkpoint = fleet.store.load(VehicleId(vehicle)).await.unwrap().0;
    assert_eq!(checkpoint.unwrap().revision, Revision(last.sequence));
    let frontier = fleet.store.frontier(partition).await.unwrap().unwrap();
    assert_eq!(frontier.sequence, last.sequence);
}

/// A deferred owner that crashes with unpersisted commits leaves its frontier
/// behind them. The restart replays those raw events from the frontier and
/// deterministically re-solves them to the same outputs (deduplicated) and the
/// same final checkpoint as a run that never crashed.
#[tokio::test(start_paused = true)]
async fn deferred_crash_replays_unpersisted_commits_identically() {
    async fn run(crash_after: Option<usize>) -> (Vec<CommittedOutput<E>>, Revision, u64) {
        let fleet = Fleet::bent_road()
            .with_commit_mode(FleetCommitMode::Deferred)
            .with_checkpoint_policy(CheckpointPolicy {
                every: 4,
                interval: Duration::from_secs(3_600),
                in_flight: 8,
            });
        let vehicle = 1u64;
        let partition = fleet.partition_of(vehicle);
        let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
        let orchestrator = fleet.spawn_orchestrator(partition);
        for (i, &p) in road_points().iter().enumerate() {
            fleet.ingest(vehicle, obs_ts(i), p).await;
        }
        let orchestrator = match crash_after {
            Some(outputs) => {
                advance_until(|| published_outputs(&fleet.bus, partition).len() >= outputs).await;
                orchestrator.crash();
                let persisted = fleet.store.load(VehicleId(vehicle)).await.unwrap().0;
                let revision = persisted.revision().map_or(0, |r| r.0);
                let frontier = fleet
                    .store
                    .frontier(partition)
                    .await
                    .unwrap()
                    .map_or(0, |f| f.sequence);
                assert!(
                    frontier <= revision,
                    "the frontier never passes the persisted checkpoint"
                );
                let last = road_points().len() as u64;
                assert!(
                    revision < last,
                    "the crash left commits unpersisted (persisted through {revision})"
                );
                fleet.spawn_orchestrator(partition)
            }
            None => orchestrator,
        };
        fleet.settle().await;
        let stats = orchestrator.stop().await;
        matcher.stop().await;
        if crash_after.is_some() {
            assert!(
                stats.committed > 0 && stats.suppressed > 0,
                "the restart both re-solved unpersisted commits and suppressed persisted ones: {stats:?}"
            );
        }
        let checkpoint = fleet
            .store
            .load(VehicleId(vehicle))
            .await
            .unwrap()
            .0
            .unwrap();
        (
            published_outputs(&fleet.bus, partition),
            checkpoint.revision,
            stats.frontier,
        )
    }

    let (clean, clean_revision, clean_frontier) = run(None).await;
    let (replayed, revision, frontier) = run(Some(road_points().len())).await;
    let content = |outputs: &[CommittedOutput<E>]| {
        outputs
            .iter()
            .map(|o| format!("{:?} {:?}", o.revision, o.kind))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        content(&replayed),
        content(&clean),
        "replay republished only deduplicated, identical outputs"
    );
    assert_eq!(revision, clean_revision);
    assert_eq!(frontier, clean_frontier);
}

/// A deferred commit whose output publish fails is retried from the owner's
/// in-memory checkpoint, which is newer than the store's, not rolled back to
/// the store: the vehicle ends exactly as an uninterrupted run does.
#[tokio::test(start_paused = true)]
async fn deferred_publish_failure_retries_from_memory() {
    use routers_realtime::bus::adapter::PublishError;
    async fn run(fail_at: Option<usize>) -> Vec<String> {
        let fleet = Fleet::bent_road()
            .with_commit_mode(FleetCommitMode::Deferred)
            .with_checkpoint_policy(CheckpointPolicy {
                every: 2,
                interval: Duration::from_secs(3_600),
                in_flight: 8,
            });
        let vehicle = 1u64;
        let partition = fleet.partition_of(vehicle);
        let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
        let orchestrator = fleet.spawn_orchestrator(partition);
        let mut last = None;
        for (i, &p) in road_points().iter().enumerate() {
            if Some(i) == fail_at {
                // Commits 0..i are in, and at least one is persisted and one
                // is not, so a store reload would rewind the vehicle.
                advance_until(|| published_outputs(&fleet.bus, partition).len() >= i).await;
                fleet.bus.fail_next_publish_on(
                    "events.matched.v1.p.>",
                    PublishError::Failed(anyhow::anyhow!("output down")),
                );
            }
            last = Some(fleet.ingest(vehicle, obs_ts(i), p).await);
        }
        fleet.settle().await;
        let stats = orchestrator.stop().await;
        matcher.stop().await;
        assert_eq!(
            stats.committed, 6,
            "{fail_at:?}: every observation committed"
        );
        // Raw sequences are bus-global, so compare decisions, and check the
        // final checkpoint against this run's own last raw event.
        let revision = fleet
            .store
            .load(VehicleId(vehicle))
            .await
            .unwrap()
            .0
            .unwrap()
            .revision;
        assert_eq!(revision, Revision(last.unwrap().sequence));
        published_outputs(&fleet.bus, partition)
            .iter()
            .map(|o| format!("{:?}", o.kind))
            .collect()
    }
    let clean = run(None).await;
    assert_eq!(run(Some(3)).await, clean);
}

/// Opening a partition takes a newer ownership epoch. An older owner still
/// running is fenced on its next frontier or checkpoint write and stops the
/// partition, and the new owner alone finishes it exactly as one owner would.
#[tokio::test(start_paused = true)]
async fn a_newer_owner_fences_the_old_one() {
    async fn run(mode: FleetCommitMode, takeover: bool) -> (Vec<String>, Revision, u64) {
        let fleet = Fleet::bent_road()
            .with_commit_mode(mode)
            .with_checkpoint_policy(CheckpointPolicy {
                every: 4,
                interval: Duration::from_secs(3_600),
                in_flight: 8,
            });
        let vehicle = 1u64;
        let partition = fleet.partition_of(vehicle);
        let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
        let first = fleet.spawn_orchestrator(partition);
        let points = road_points();
        let mut last = None;
        for (i, &p) in points.iter().enumerate().take(3) {
            last = Some(fleet.ingest(vehicle, obs_ts(i), p).await);
        }
        advance_until(|| published_outputs(&fleet.bus, partition).len() >= 3).await;

        let owner = if takeover {
            let second = fleet.spawn_orchestrator(partition);
            advance_until(|| first.is_finished()).await;
            let error = first.join().await.expect_err("the old owner must stop");
            assert!(
                error.to_string().contains("ownership lost"),
                "{mode:?}: unexpected exit: {error:#}"
            );
            second
        } else {
            first
        };
        for (i, &p) in points.iter().enumerate().skip(3) {
            last = Some(fleet.ingest(vehicle, obs_ts(i), p).await);
        }
        fleet.settle().await;
        let stats = owner.stop().await;
        matcher.stop().await;
        let checkpoint = fleet
            .store
            .load(VehicleId(vehicle))
            .await
            .unwrap()
            .0
            .unwrap();
        assert_eq!(checkpoint.revision, Revision(last.unwrap().sequence));
        let content = published_outputs(&fleet.bus, partition)
            .iter()
            .map(|o| format!("{:?} {:?}", o.revision, o.kind))
            .collect();
        (content, checkpoint.revision, stats.frontier)
    }

    for mode in [FleetCommitMode::Deferred, FleetCommitMode::Direct] {
        let single = run(mode, false).await;
        let handed_over = run(mode, true).await;
        assert_eq!(handed_over, single, "{mode:?}: the takeover is invisible");
    }
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
    advance_until(|| !fleet.bus.published(REQUESTS).is_empty()).await;

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

/// A prepare failure leaves no durable commit. The worker must roll back and
/// retry the same raw rather than treating an empty prepared slot as success.
#[tokio::test(start_paused = true)]
async fn prepare_failure_retries_without_acknowledging_undurable_raw() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    fleet.store.fail_next(Op::Prepare);

    let matcher = fleet.spawn_matcher(MatcherBehaviour::scripted_layer());
    let orchestrator = fleet.spawn_orchestrator(partition);
    let observation = fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;

    fleet.settle().await;
    let stats = orchestrator.stop().await;
    matcher.stop().await;

    assert_eq!(stats.committed, 1, "the retried decision became durable");
    assert_eq!(stats.frontier, observation.sequence);
    assert_eq!(published_outputs(&fleet.bus, partition).len(), 1);
    let checkpoint = fleet
        .store
        .load(VehicleId(vehicle))
        .await
        .unwrap()
        .0
        .expect("checkpoint committed after retry");
    assert_eq!(checkpoint.revision, Revision(observation.sequence));
}

/// An ack-wait copy arriving while the accepted result's promote is blocked
/// must not retire the broker record before durability is proven.
#[tokio::test(start_paused = true)]
async fn duplicate_result_during_blocked_commit_is_not_acknowledged() {
    let fleet = Fleet::bent_road().with_blocked_retry(Duration::from_secs(60));
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    let result_subject = result_subject(partition);
    fleet.store.fail_next(Op::Promote);

    let matcher = fleet.spawn_matcher(MatcherBehaviour::scripted_layer());
    let orchestrator = fleet.spawn_orchestrator(partition);
    fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;
    advance_until(|| !published_outputs(&fleet.bus, partition).is_empty()).await;

    fleet.redeliver_results(partition);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(
        fleet.bus.acked_count(&result_subject),
        0,
        "neither the retained original nor its ack-wait copy is acked",
    );

    orchestrator.crash();
    matcher.stop().await;
    let restarted = fleet.spawn_orchestrator(partition);
    fleet.settle().await;
    let _ = restarted.stop().await;

    assert_eq!(published_outputs(&fleet.bus, partition).len(), 1);
    assert!(
        fleet
            .store
            .load(VehicleId(vehicle))
            .await
            .unwrap()
            .0
            .is_some(),
        "recovery promoted the surviving prepared commit",
    );
}
