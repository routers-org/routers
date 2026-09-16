//! Steady-state pipeline behaviour: the happy path end to end, transport
//! duplicate coalescing, result idempotency, and early-result parking.

use super::fixture::*;

/// Six observations solve to six matched outputs with climbing revisions, one
/// materialised layer per stamp, and the frontier at the last raw sequence.
#[tokio::test(start_paused = true)]
async fn happy_path_matches_and_finalizes() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    let matcher = fleet.spawn_matcher(MatcherBehaviour::engine());
    let (materializer, sink) = fleet.spawn_materializer();
    let orchestrator = fleet.spawn_orchestrator(partition);

    let points = road_points();
    let mut ids = Vec::new();
    for (i, &p) in points.iter().enumerate() {
        ids.push(fleet.ingest(vehicle, obs_ts(i), p).await);
    }
    let last_seq = ids.last().unwrap().sequence;

    fleet.settle().await;
    let stats = orchestrator.stop().await;
    matcher.stop().await;
    let _ = materializer.stop().await;

    assert_eq!(stats.accepted, 6, "every observation was answered");
    assert_eq!(stats.committed, 6, "every answer committed");
    assert_eq!(stats.terminal, 0, "no deadline fired");
    assert_eq!(stats.resets, 0, "a continuous trace never resets");
    assert_eq!(
        stats.frontier, last_seq,
        "frontier at the last raw sequence"
    );

    let outputs = published_outputs(&fleet.bus, partition);
    let matched: Vec<&CommittedOutput<E>> = outputs
        .iter()
        .filter(|o| matches!(o.kind, OutputKind::Matched { .. }))
        .collect();
    assert_eq!(matched.len(), 6, "six matched outputs");
    for pair in matched.windows(2) {
        assert!(
            pair[1].revision.0 > pair[0].revision.0,
            "revisions climb monotonically: {:?} then {:?}",
            pair[0].revision,
            pair[1].revision,
        );
    }

    let view = sink
        .snapshot(VehicleId(vehicle))
        .expect("vehicle materialised");
    let segment = &view.segments[&view.current];
    let got: Vec<i64> = segment.layers.keys().copied().collect();
    let want: Vec<i64> = (0..points.len()).map(obs_ts).collect();
    assert_eq!(got, want, "a layer at each observation's timestamp");
    assert_eq!(segment.last_revision, Some(Revision(last_seq)));
}

/// A raw redelivered while its original is still in flight coalesces: no second
/// job or output.
#[tokio::test(start_paused = true)]
async fn duplicate_raw_delivery_is_coalesced_not_reprocessed() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    // No matcher: the observation dispatches and stays in flight.
    let orchestrator = fleet.spawn_orchestrator(partition);
    fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;

    advance_until(|| !fleet.bus.published("solve.v1.g.>").is_empty()).await;
    fleet.redeliver_raw(partition);
    advance_until(|| fleet.bus.acked_count(&raw_subject_of(partition)) >= 1).await;

    let stats = orchestrator.stop().await;

    assert_eq!(stats.observed, 2, "the observation was delivered twice");
    assert_eq!(stats.coalesced, 1, "the redelivery coalesced");
    assert_eq!(stats.dispatched, 1, "only one job was dispatched");
    assert!(
        published_outputs(&fleet.bus, partition).is_empty(),
        "no output while the single job is still in flight",
    );
}

/// A duplicate delivery of an already-decided result is rejected out of the commit path.
#[tokio::test(start_paused = true)]
async fn duplicate_result_delivery_is_idempotent() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    // No live matcher: we answer by hand so we control the duplicate exactly.
    let orchestrator = fleet.spawn_orchestrator(partition);
    let observation = fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;

    advance_until(|| !fleet.bus.published("solve.v1.g.>").is_empty()).await;

    let identity = fleet.fresh_identity(vehicle, observation);
    fleet.publish_result(identity.clone(), empty_solved()).await;
    fleet
        .publish_result_dup(identity, empty_solved(), "dup")
        .await;
    fleet.redeliver_results(partition);

    fleet.settle().await;
    let stats = orchestrator.stop().await;

    assert_eq!(stats.accepted, 1, "the first answer was accepted");
    assert_eq!(stats.committed, 1, "exactly one commit");
    assert!(
        stats.rejected + stats.quarantined > 0,
        "the duplicate was kept out of the commit path (rejected {} / quarantined {})",
        stats.rejected,
        stats.quarantined,
    );
    assert_eq!(
        published_outputs(&fleet.bus, partition).len(),
        1,
        "the output was not doubled",
    );
}

/// A result that reaches the worker before its job is dispatched is parked, then
/// accepted when the dispatch lands — one output, no duplicate.
#[tokio::test(start_paused = true)]
async fn early_result_is_parked_then_accepted() {
    use routers_realtime::bus::adapter::PublishError;

    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    let orchestrator = fleet.spawn_orchestrator(partition);

    // Arm a one-shot job-publish failure so the vehicle is held tracked-but-undispatched, and the answer parks.
    let observation = fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;
    let identity = fleet.fresh_identity(vehicle, observation);
    fleet.publish_result(identity, empty_solved()).await;
    fleet
        .bus
        .fail_next_publish(PublishError::Failed(anyhow::anyhow!("job publish down")));

    fleet.settle().await;
    let stats = orchestrator.stop().await;

    assert!(stats.parked >= 1, "the early answer was parked");
    assert_eq!(stats.accepted, 1, "then accepted on dispatch");
    assert_eq!(stats.committed, 1, "exactly one commit");
    assert_eq!(
        published_outputs(&fleet.bus, partition).len(),
        1,
        "one output for the parked-then-accepted answer",
    );
}

/// The concrete raw subject for a partition.
fn raw_subject_of(partition: u16) -> String {
    routers_realtime::topology::raw_subject(u64::from(partition))
}
