//! Steady-state pipeline behaviour: the happy path end to end, transport
//! duplicate coalescing, result idempotency, and early-result parking.

use super::fixture::*;
use core::time::Duration;
use routers_realtime::orchestrator::admission::Admission;

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
    assert_eq!(stats.terminal, 0, "no terminal was synthesized");
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

/// A raw redelivered while its original is still in flight remains broker-owned;
/// crashing before commit therefore lets restart process it exactly once.
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
    advance_until(|| !fleet.bus.nak_delays().is_empty()).await;

    assert_eq!(
        fleet.bus.acked_count(&raw_subject_of(partition)),
        0,
        "the duplicate cannot retire the original before commit",
    );
    orchestrator.crash();

    let matcher = fleet.spawn_matcher(MatcherBehaviour::scripted_layer());
    let restarted = fleet.spawn_orchestrator(partition);
    fleet.settle().await;
    let stats = restarted.stop().await;
    matcher.stop().await;

    assert_eq!(stats.committed, 1);
    assert_eq!(published_outputs(&fleet.bus, partition).len(), 1);
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

    let job = published_jobs(&fleet.bus)
        .into_iter()
        .find(|job| job.identity.observation == observation)
        .expect("observation job published");
    fleet.publish_result(&job, empty_solved()).await;
    fleet.publish_result_dup(&job, empty_solved(), "dup").await;
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

/// A result retained while replay is admission-held stays broker-owned across
/// a crash, then matches the byte-identical rebuilt job after restart.
#[tokio::test(start_paused = true)]
async fn early_result_is_parked_then_accepted() {
    let mut fleet = Fleet::bent_road();
    let vehicle = 1u64;
    let partition = fleet.partition_of(vehicle);
    let result_subject = routers_realtime::topology::result_subject(u64::from(partition));

    // First publish the authentic job envelope, then model a crash before its
    // result is consumed.
    let first = fleet.spawn_orchestrator(partition);
    let observation = fleet.ingest(vehicle, obs_ts(0), road_points()[0]).await;
    advance_until(|| !published_jobs(&fleet.bus).is_empty()).await;
    let job = published_jobs(&fleet.bus)
        .into_iter()
        .find(|job| job.identity.observation == observation)
        .expect("observation job published");
    first.crash();

    // Replay owns the raw but cannot dispatch yet, so the authentic result is
    // parked with its delivery handle still unacknowledged.
    fleet.admission = Admission::new(
        AdmissionConfig {
            global_jobs: 0,
            region_jobs: 0,
            ..AdmissionConfig::default()
        },
        fleet.catalog.regions.iter().map(|region| &region.id),
    );
    fleet.publish_result(&job, empty_solved()).await;
    let held = fleet.spawn_orchestrator(partition);
    tokio::time::sleep(Duration::from_millis(1)).await;
    assert_eq!(fleet.bus.acked_count(&result_subject), 0);
    held.crash();

    fleet.admission = Admission::new(
        AdmissionConfig::default(),
        fleet.catalog.regions.iter().map(|region| &region.id),
    );
    let restarted = fleet.spawn_orchestrator(partition);

    fleet.settle().await;
    let stats = restarted.stop().await;

    assert_eq!(stats.accepted, 1, "the replayed answer matched rebuilt job");
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
