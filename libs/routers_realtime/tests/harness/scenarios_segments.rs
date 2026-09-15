//! Segment continuity: state loss, teleport, regression suppression, and
//! unserved coverage — the §8 segment rules and the §10 rows they own. (T31)

use super::fixture::*;

/// The topology subjects, fully qualified so the scenarios stay terse.
fn raw_subject(partition: u16) -> String {
    routers_realtime::topology::raw_subject(u64::from(partition))
}

/// After a run leaves finalised history, corrupting the committed checkpoint
/// makes the next observation open a fresh segment with a `Reset { StateLost }`;
/// the old segment's layers are untouched.
///
/// State loss is modelled by an *undecodable surviving* checkpoint (a decode
/// failure is what the merged `restore_vehicle` classifies as state loss); a
/// fully-dropped checkpoint would read as a genuinely fresh vehicle and open a
/// new segment without a reset.
#[tokio::test(start_paused = true)]
async fn checkpoint_loss_opens_new_segment_keeps_finalized_history() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    let points = road_points();

    let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
    let (materializer, sink) = fleet.spawn_materializer();

    // Build up four committed observations of real, layered history.
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

    // Corrupt the committed checkpoint: an undecodable blob at a superseding
    // revision, installed via a throwaway bus so no stray output lands.
    fleet
        .seed_checkpoint_bytes(
            vehicle,
            partition,
            5,
            Some(Revision(4)),
            b"not a checkpoint".to_vec(),
        )
        .await;

    // The next observation restores the corrupt state, resets, and re-matches.
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

    // A Reset { StateLost } then a Matched, in that order.
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

    // The old segment's history survived, and a new segment carries the reset's
    // fresh match.
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

/// A point more than the jump threshold from the last committed origin resets
/// the segment as a teleport, then matches in the new segment.
#[tokio::test(start_paused = true)]
async fn teleport_resets_segment() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    let points = road_points();

    // A real engine so the committed checkpoint carries the last origin the
    // teleport is measured against; the two ends of the staircase are ~2.8 km
    // apart, beyond the 2 km jump threshold.
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

/// An observation whose sequence is behind the partition's completion frontier —
/// a late, out-of-order delivery of something already moved past — is suppressed
/// and acked without a job.
///
/// The merged reader suppresses regressions by the completion frontier
/// (sequence), the durable "already decided" mark, rather than by comparing
/// wall-clock stamps, so the scenario drives it through the frontier.
#[tokio::test(start_paused = true)]
async fn timestamp_regression_is_suppressed() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    // The partition has already committed up to sequence 5.
    fleet
        .store
        .set_frontier(routers_realtime::store::checkpoint::PartitionFrontier {
            partition,
            sequence: 5,
        })
        .await
        .unwrap();

    let orchestrator = fleet.spawn_orchestrator(partition);
    // The first raw takes sequence 1 — behind the frontier, so a regression.
    let observation = fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;
    assert!(observation.sequence <= 5, "the raw is behind the frontier");

    fleet.settle().await;
    let stats = orchestrator.stop().await;

    assert_eq!(
        stats.dispatched, 0,
        "no job was dispatched for the regression"
    );
    assert!(stats.suppressed >= 1, "the regression was suppressed");
    assert!(
        published_outputs(&fleet.bus, partition).is_empty(),
        "no output for a suppressed regression",
    );
    assert!(
        fleet.bus.acked_count(&raw_subject(partition)) >= 1,
        "the suppressed raw was acked",
    );
}

/// A point outside every region's coverage is terminated as unsupported
/// coverage, without a solve job.
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
