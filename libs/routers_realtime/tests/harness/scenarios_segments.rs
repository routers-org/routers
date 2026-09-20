//! Segment continuity: state loss, teleport, regression suppression, and
//! unserved coverage — the §8 segment rules and the §10 rows they own.

use super::fixture::*;

/// Corrupting the committed checkpoint makes the next observation open a fresh
/// `Reset { StateLost }` segment, leaving the old layers untouched. State loss is
/// modelled by an undecodable checkpoint (a dropped one would read as fresh, no reset).
#[tokio::test(start_paused = true)]
async fn checkpoint_loss_opens_new_segment_keeps_finalized_history() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    let points = road_points();

    let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
    let (materializer, sink) = fleet.spawn_materializer();

    let run = fleet.spawn_orchestrator(partition);
    for (i, &p) in points.iter().enumerate().take(4) {
        fleet.ingest(vehicle, obs_ts(i), p).await;
    }
    fleet.settle().await;
    let stats = run.stop().await;
    assert_eq!(stats.committed, 4, "the first segment committed cleanly");

    let before = sink.snapshot(VehicleId(vehicle)).expect("vehicle present");
    let old_segment = before.current;
    let old_layers: Vec<i64> = before.segments[&old_segment]
        .layers
        .keys()
        .copied()
        .collect();
    assert_eq!(old_layers.len(), 4, "four layers of history so far");

    // Corrupt the checkpoint: an undecodable blob at a superseding revision.
    fleet
        .seed_checkpoint_bytes(
            vehicle,
            partition,
            5,
            Some(Revision(4)),
            b"not a checkpoint".to_vec(),
        )
        .await;

    let restarted = fleet.spawn_orchestrator(partition);
    let lost_obs = fleet.ingest(vehicle, obs_ts(4), points[4]).await;
    fleet.settle().await;
    let stats = restarted.stop().await;
    matcher.stop().await;
    let _ = materializer.stop().await;

    assert_eq!(
        stats.resets, 1,
        "state loss opened exactly one fresh segment"
    );

    let outputs = published_outputs(&fleet.bus, partition);
    assert!(
        outputs.iter().any(|o| matches!(
            o.kind,
            OutputKind::Reset {
                reason: ResetReason::StateLost,
                ..
            }
        )),
        "a StateLost reset was emitted",
    );

    let after = sink.snapshot(VehicleId(vehicle)).expect("vehicle present");
    let new_segment = SegmentId(lost_obs.sequence);
    assert_ne!(new_segment, old_segment, "the reset opened a new segment");
    assert_eq!(
        after.current, new_segment,
        "new work lands in the new segment"
    );
    assert_eq!(
        after.segments[&old_segment]
            .layers
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        old_layers,
        "the old segment's finalised layers are untouched",
    );
    assert!(
        after.segments[&new_segment].layers.contains_key(&obs_ts(4)),
        "the new segment holds the fresh match",
    );
}

/// A point beyond the jump threshold resets the segment as a teleport, then matches anew.
#[tokio::test(start_paused = true)]
async fn teleport_resets_segment() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    let points = road_points();

    // The staircase ends are ~2.8 km apart, beyond the 2 km jump threshold.
    let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
    let (materializer, sink) = fleet.spawn_materializer();
    let orchestrator = fleet.spawn_orchestrator(partition);

    fleet.ingest(vehicle, obs_ts(0), points[0]).await;
    fleet.settle().await;

    let teleport = fleet.ingest(vehicle, obs_ts(1), points[5]).await;
    fleet.settle().await;

    let stats = orchestrator.stop().await;
    matcher.stop().await;
    let _ = materializer.stop().await;

    assert_eq!(stats.resets, 1, "the jump reset the segment once");
    let outputs = published_outputs(&fleet.bus, partition);
    assert!(
        outputs.iter().any(|o| matches!(
            o.kind,
            OutputKind::Reset {
                reason: ResetReason::Teleport,
                ..
            }
        )),
        "a Teleport reset was emitted",
    );

    let view = sink.snapshot(VehicleId(vehicle)).expect("vehicle present");
    assert_eq!(
        view.current,
        SegmentId(teleport.sequence),
        "new work lands in the post-teleport segment",
    );
    assert!(
        view.segments.len() >= 2,
        "the pre-teleport segment is retained alongside the new one",
    );
}

/// New raw sequences whose supplier timestamps do not advance the same
/// vehicle's committed origin become durable terminals. The checkpoint and
/// frontier advance, while the retained trip remains unchanged.
#[tokio::test(start_paused = true)]
async fn newer_sequences_with_older_or_equal_timestamps_commit_terminals() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    let point = road_points()[0];
    let committed_ts = obs_ts(1);

    let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
    let orchestrator = fleet.spawn_orchestrator(partition);
    let initial = fleet.ingest(vehicle, committed_ts, point).await;
    fleet.settle().await;

    let older = fleet.ingest(vehicle, committed_ts - 1, point).await;
    let equal = fleet
        .ingest_with_msg_id(
            vehicle,
            committed_ts,
            point,
            Some("equal-timestamp-new-sequence"),
        )
        .await;
    assert!(older.sequence > initial.sequence);
    assert!(equal.sequence > older.sequence);

    fleet.settle().await;
    let stats = orchestrator.stop().await;
    matcher.stop().await;

    assert_eq!(
        stats.dispatched, 1,
        "only the monotonic observation was solved"
    );
    assert_eq!(
        stats.committed, 3,
        "both regressions were durably committed"
    );
    assert_eq!(stats.terminal, 2);
    assert_eq!(stats.frontier, equal.sequence);

    let outputs = published_outputs(&fleet.bus, partition);
    assert_eq!(
        outputs
            .iter()
            .filter(|output| matches!(
                output.kind,
                OutputKind::Terminal {
                    reason: TerminalReason::TimestampRegression,
                    ..
                }
            ))
            .count(),
        2,
    );

    let stored = fleet.store.snapshot().checkpoints[&VehicleId(vehicle)].clone();
    let checkpoint =
        routers_realtime::store::checkpoint::VehicleCheckpoint::<E>::decode(&stored.bytes).unwrap();
    assert_eq!(checkpoint.last_input, equal);
    assert_eq!(checkpoint.revision, Revision(equal.sequence));
    assert_eq!(
        checkpoint.trip.origins().last().unwrap().timestamp,
        committed_ts,
        "terminal regressions advance durable input without changing the trip",
    );
}

/// A point outside every region's coverage is terminated as unsupported coverage, no solve job.
#[tokio::test(start_paused = true)]
async fn unserved_cell_is_terminal_unsupported_coverage() {
    use geo::Point;

    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    // Null island: a valid point far from the bent road, in no served cell.
    let elsewhere = Point::new(0.0, 0.0);
    let (materializer, _sink) = fleet.spawn_materializer();
    let orchestrator = fleet.spawn_orchestrator(partition);
    fleet.ingest(vehicle, obs_ts(0), elsewhere).await;

    fleet.settle().await;
    let stats = orchestrator.stop().await;
    let _ = materializer.stop().await;

    assert_eq!(stats.dispatched, 0, "no solve job for an unserved point");
    assert_eq!(stats.terminal, 1, "one terminal was committed");

    let outputs = published_outputs(&fleet.bus, partition);
    assert_eq!(outputs.len(), 1, "exactly one output");
    assert!(
        matches!(
            outputs[0].kind,
            OutputKind::Terminal {
                reason: TerminalReason::UnsupportedCoverage,
                ..
            }
        ),
        "the terminal is tagged unsupported coverage",
    );
}
