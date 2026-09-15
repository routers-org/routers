//! Steady-state pipeline behaviour: the happy path end to end, transport
//! duplicate coalescing, result idempotency, and early-result parking. (T31)
//!
//! These pin the §10 rows the orchestrator owns for *nominal* traffic: a clean
//! trace materialises into a monotone history, a redelivered raw is coalesced
//! rather than re-solved, a duplicate answer never doubles an output, and an
//! answer that outruns its dispatch is parked and then accepted exactly once.

use super::fixture::*;

/// Six clustered observations on the bent road solve to six matched outputs with
/// strictly climbing revisions; the materialised trace carries a layer at each
/// observation's stamp, and the frontier ends at the last raw sequence.
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

    // Exactly six Matched outputs, revisions strictly increasing.
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

    // The materialised trace holds one layer per observation stamp.
    let view = sink
        .snapshot(VehicleId(vehicle))
        .expect("vehicle materialised");
    let segment = &view.segments[&view.current];
    let got: Vec<i64> = segment.layers.keys().copied().collect();
    let want: Vec<i64> = (0..points.len()).map(obs_ts).collect();
    assert_eq!(got, want, "a layer at each observation's timestamp");
    assert_eq!(segment.last_revision, Some(Revision(last_seq)));
}

/// A raw message redelivered while its original is still in flight coalesces:
/// no second job, no second output.
#[tokio::test(start_paused = true)]
async fn duplicate_raw_delivery_is_coalesced_not_reprocessed() {
    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    // No matcher: the observation dispatches and stays in flight, so a
    // redelivery coalesces against the still-outstanding original.
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

/// A duplicate delivery of an already-decided result is rejected (or
/// quarantined) out of the commit path; the committed output is unchanged.
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
    // The real answer, then a duplicate of it under a distinct dedup key so the
    // broker delivers it a second time.
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
/// replayed and accepted when the dispatch lands — one output, no duplicate.
///
/// A result only parks (rather than dropping) once its vehicle is *tracked*, so
/// the scenario tracks the vehicle and holds its first dispatch: a one-shot
/// job-publish failure leaves the vehicle enqueued but undispatched, the answer
/// parks against it, and the housekeeping retry then dispatches and drains the
/// park. Every set-up call runs while the worker task is still parked (the
/// cooperative single-thread runtime only advances it when the test awaits), so
/// the ordering is deterministic.
#[tokio::test(start_paused = true)]
async fn early_result_is_parked_then_accepted() {
    use routers_realtime::bus::adapter::PublishError;

    let fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);

    let orchestrator = fleet.spawn_orchestrator(partition);

    // Publish the raw and its answer while the worker is still parked, then arm a
    // one-shot job-publish failure so the worker's *first* publish — the solve
    // job — fails and the vehicle is held, tracked but undispatched.
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

/// The concrete raw subject for a partition — a tiny shim so the scenarios read
/// cleanly without importing the topology functions directly.
fn raw_subject_of(partition: u16) -> String {
    routers_realtime::topology::raw_subject(u64::from(partition))
}
