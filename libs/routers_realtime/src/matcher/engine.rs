//! Matcher: the solve engine.
//!
//! [`Engine`] is the map-matching core lifted out of `bin/matcher.rs` so the
//! pull loop (T27), the validator (T25) and the tests can drive a solve
//! without a NATS connection or a concrete OSM network. It owns exactly the
//! heavy, shareable state a solve reaches into — the loaded network, its
//! runtime, the costing strategies and the reachability cache — so a whole
//! pool of blocking solves can fan out over one `Arc<Engine>` with nothing to
//! serialise on. A per-solve [`Matcher`] is a cheap bundle of borrows into
//! this and is rebuilt each call.
//!
//! The engine is generic over the [`Network`] rather than pinned to the OSM
//! types the binary uses: the tests exercise it against
//! `routers_network::mock::MockNetwork`, and the binary specialises it to
//! its sharded network in T32.
//!
//! # Outcomes, not replies
//!
//! Where the binary answered every request with a single opaque reply (a match
//! or a bare "no match"), the engine returns a typed [`SolveOutcome`]: the nominal
//! terminals ([`SolveOutcome::Unanchored`], [`SolveOutcome::Disconnected`])
//! and the faults ([`SolveOutcome::Internal`]) are distinct, so the owner can
//! commit the right terminal reason instead of collapsing them into "nothing
//! to emit". Everything the engine does not decide — the deadline (T25) and
//! the decoded-size bound (T27) — is left to the caller; the engine never
//! rejects a job, it only solves the one it is handed.

use alloc::sync::Arc;
use core::future::Future;

use log::{debug, error, warn};
use routers_network::{Entry, Network};
use routers_transition::{
    Continuation, MatchError, Matcher,
    costing::{CostingStrategies, DefaultEmissionCost, DefaultTransitionCost},
    layer::generation::StandardGenerator,
    primitives::PredicateCache,
    weigh::AllCompute,
};
use tracing::{field, info_span};

use crate::event::MatchedDiff;
use crate::protocol::job::SolveJob;
use crate::protocol::result::SolveOutcome;

/// The shared, reusable state one region's matcher solves against.
///
/// Everything here is owned so the engine can be wrapped in an `Arc` and
/// shared across concurrent blocking solves without juggling `'static`
/// borrows: the network's spatial index and the predicate cache are the only
/// heavy state and both are shared, while a per-solve [`Matcher`] is just a
/// bundle of borrows that is free to build.
pub struct Engine<N: Network> {
    network: Arc<N>,
    runtime: N::Runtime,
    costing: CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, N::Entry>,
    cache: Arc<PredicateCache<N>>,
    search_distance: Option<f64>,
}

impl<N: Network> Engine<N> {
    /// Build an engine over a loaded `network` and its `runtime`, using the
    /// default costing strategies and a fresh reachability cache.
    ///
    /// `search_distance` overrides the candidate generator's default reach
    /// when `Some`; `None` keeps the generator's own default. The runtime is
    /// the one that produced `network` — the cache binds itself to it on first
    /// query and is only valid for that runtime.
    pub fn new(network: Arc<N>, runtime: N::Runtime, search_distance: Option<f64>) -> Self {
        Self {
            network,
            runtime,
            costing: CostingStrategies::default(),
            cache: Arc::new(PredicateCache::default()),
            search_distance,
        }
    }

    /// Solve one job, returning the typed outcome the owner commits against.
    ///
    /// The whole solve is recorded onto a fresh `match_event` span so a trace
    /// carries the continuation kind, the convergence layer and the emitted
    /// layer count without a separate metric.
    ///
    /// A [`Continuation::Resume`] whose trip references edges this network does
    /// not serve (a resume solved on another shard) is *downgraded* to a
    /// restart over the trip's own observations, with `diff.downgraded` set:
    /// the emission re-covers them under a higher revision so the reconciled
    /// history heals the seam. Fresh observations that anchor to no road are
    /// dropped; a solve left with nothing to match is [`SolveOutcome::Unanchored`].
    ///
    /// The deadline and the decoded-size bound are *not* checked here — the
    /// validator (T25) and the pull loop (T27) own those. The engine only
    /// solves the job it is handed.
    pub fn solve(&self, job: &SolveJob<N::Entry>) -> SolveOutcome<N::Entry> {
        let vehicle_id = job.identity.vehicle_id;

        let mut generator = StandardGenerator::new(self.network.as_ref(), &self.costing.emission);
        if let Some(distance) = self.search_distance {
            generator = generator.with_search_distance(distance);
        }

        let weigher = AllCompute::default().use_cache(self.cache.clone());
        let matcher = Matcher::new(
            self.network.as_ref(),
            &self.costing,
            generator,
            weigher,
            &self.runtime,
        );

        let span = info_span!(
            "match_event",
            outcome = field::Empty,
            severity = field::Empty,
            continuation = field::Empty,
            converged = field::Empty,
            emitted = field::Empty,
        );
        let _entered = span.enter();

        let (mut trip, fresh, downgraded) = match job.context.clone() {
            // A resume solved on another shard references edges this shard's
            // padding may not cover: adopting it would route through nodes that
            // do not exist here. Degrade to a restart over the trip's own
            // observations — the emission re-covers them under a higher
            // revision, so the reconciled history heals the seam.
            Continuation::Resume { trip, fresh } if !matcher.supports(&trip) => {
                span.record("continuation", "downgrade");
                warn!("{vehicle_id}: resume references a foreign shard; restarting");

                let fresh = trip.origins().iter().copied().chain(fresh).collect();
                (matcher.begin(), fresh, true)
            }
            Continuation::Resume { trip, fresh } => {
                span.record("continuation", "resume");
                (trip, fresh, false)
            }
            Continuation::Restart { fresh } => {
                span.record("continuation", "restart");
                (matcher.begin(), fresh, false)
            }
        };

        info_span!("push", points = fresh.len()).in_scope(|| {
            for origin in fresh {
                match matcher.push(&mut trip, origin) {
                    Ok(_) => {}
                    Err(MatchError::Unanchored(err)) => {
                        info_span!("point_drop", reason = "unanchored")
                            .in_scope(|| debug!("{vehicle_id}: dropped off-network point ({err})"));
                    }
                    Err(err) => {
                        info_span!("point_drop", reason = "push_error")
                            .in_scope(|| error!("{vehicle_id}: could not push point: {err}"));
                    }
                }
            }
        });

        if trip.is_empty() {
            span.record("outcome", "no_anchor");
            span.record("severity", "nominal");
            warn!("{vehicle_id}: no anchored layers to solve");
            return SolveOutcome::Unanchored;
        }

        if let Err(err) = info_span!("solve").in_scope(|| matcher.solve(&mut trip)) {
            let (outcome, severity) = classify(&err);
            span.record("outcome", outcome);
            span.record("severity", severity);
            error!("{vehicle_id}: unable to solve trip");
            return terminal_outcome(err);
        }

        // Copied out: the snapshot's borrow spans the whole trip mutably.
        let origins = trip.origins().to_vec();

        let solution = match info_span!("snapshot").in_scope(|| matcher.snapshot(&mut trip)) {
            Ok(solution) => solution,
            Err(err) => {
                let (outcome, severity) = classify(&err);
                span.record("outcome", outcome);
                span.record("severity", severity);
                return terminal_outcome(err);
            }
        };

        // Emit everything a future solve could still change — the whole trip
        // since its last cut. Revision 0 is a placeholder: the owner stamps the
        // real revision (the raw stream sequence) before publishing.
        let mut diff = info_span!("emit")
            .in_scope(|| MatchedDiff::new(&solution, &origins, self.network.as_ref(), 0));
        diff.downgraded = downgraded;
        drop(solution);
        span.record("emitted", diff.layers.len());

        // The convergence timestamp is read from `origins` *before* the cut:
        // once the trip is tailed, its layers are renumbered, but the copied
        // origins still index by the pre-cut layer. The convergence layer is
        // the latest no future push can change; cutting behind it drops layers
        // that are final and already emitted, leaving the convergence layer as
        // the resume anchor. An unfused trip stays whole — the orchestrator's
        // context window bounds its growth.
        let converged_through = match matcher.convergence(&trip) {
            Ok(Some(layer)) => {
                span.record("converged", layer.index() as u64);
                let timestamp = origins[layer.index()].timestamp;
                trip.tail(trip.layers() - layer.index());
                Some(timestamp)
            }
            Ok(None) => None,
            Err(err) => {
                error!("{vehicle_id}: convergence query failed: {err}");
                None
            }
        };

        span.record("outcome", "success");
        span.record("severity", "ok");

        SolveOutcome::Solved {
            diff,
            trip,
            converged_through,
        }
    }
}

impl<N> Engine<N>
where
    N: Network + 'static,
    N::Runtime: 'static,
{
    /// Solve `job` on the blocking pool, awaited as a future.
    ///
    /// Solving is synchronous and CPU-bound, so it runs on
    /// [`tokio::task::spawn_blocking`] rather than blocking a runtime worker.
    /// A panic inside the solve is caught and mapped to
    /// [`SolveOutcome::Internal`] so one poisoned job never takes the pull loop
    /// down with it.
    pub fn solve_blocking(
        self: &Arc<Self>,
        job: SolveJob<N::Entry>,
    ) -> impl Future<Output = SolveOutcome<N::Entry>> {
        let engine = Arc::clone(self);
        async move {
            tokio::task::spawn_blocking(move || engine.solve(&job))
                .await
                .unwrap_or_else(|err| {
                    error!("solve task panicked: {err}");
                    SolveOutcome::Internal {
                        reason: "panic".to_owned(),
                    }
                })
        }
    }
}

/// A match attempt's `outcome`/`severity` span labels for the success-ratio
/// series. Nominal failures are the data's fault (a point off every road, a
/// trace the network cannot bridge) and expected in healthy operation; fatal
/// ones are ours.
fn classify(err: &MatchError) -> (&'static str, &'static str) {
    match err {
        MatchError::Unanchored(_) => ("unanchored", "nominal"),
        MatchError::Disconnected(_) => ("disconnected", "nominal"),
        MatchError::TrellisError(_) | MatchError::SolveError(_) => ("internal", "fatal"),
    }
}

/// Project a solve failure onto its terminal [`SolveOutcome`]. The two nominal
/// failures leave committed state intact; a trellis or solver error is the
/// matcher's own fault and carries its message for the logs (never a label).
fn terminal_outcome<E: Entry>(err: MatchError) -> SolveOutcome<E> {
    match err {
        MatchError::Unanchored(_) => SolveOutcome::Unanchored,
        MatchError::Disconnected(_) => SolveOutcome::Disconnected,
        err @ (MatchError::TrellisError(_) | MatchError::SolveError(_)) => SolveOutcome::Internal {
            reason: err.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use geo::{Point, point};
    use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
    use routers_transition::Origin;

    use crate::event::VehicleId;
    use crate::protocol::ids::{GraphVersion, Lane, ObservationId, RegionId, SCHEMA_VERSION};
    use crate::protocol::job::{JobIdentity, SolveJob};

    /// A staircase road (west, south, west) — the same shape the crate's
    /// `matched_diff` integration test matches against.
    fn bent_road() -> MockNetwork {
        MockNetworkBuilder::new()
            .node(1, point!(x: -118.15, y: 34.15))
            .node(2, point!(x: -118.16, y: 34.15))
            .node(3, point!(x: -118.17, y: 34.15))
            .node(4, point!(x: -118.17, y: 34.14))
            .node(5, point!(x: -118.18, y: 34.14))
            .edge(1, 2)
            .edge(2, 3)
            .edge(3, 4)
            .edge(4, 5)
            .build()
    }

    /// The same road geometry with a *disjoint* node-id space: its candidates
    /// reference edges no [`bent_road`] network serves, so a trip solved here
    /// fails `matcher.supports` on a `bent_road` engine and forces a downgrade.
    fn bent_road_disjoint() -> MockNetwork {
        MockNetworkBuilder::new()
            .node(1001, point!(x: -118.15, y: 34.15))
            .node(1002, point!(x: -118.16, y: 34.15))
            .node(1003, point!(x: -118.17, y: 34.15))
            .node(1004, point!(x: -118.17, y: 34.14))
            .node(1005, point!(x: -118.18, y: 34.14))
            .edge(1001, 1002)
            .edge(1002, 1003)
            .edge(1003, 1004)
            .edge(1004, 1005)
            .build()
    }

    /// Six spaced observations tracing the bend, with realistic supplier
    /// stamps: distinct, five seconds apart, non-zero micros.
    fn observations() -> Vec<Origin> {
        [
            point!(x: -118.151, y: 34.1503),
            point!(x: -118.155, y: 34.1503),
            point!(x: -118.165, y: 34.1503),
            point!(x: -118.170, y: 34.1490),
            point!(x: -118.172, y: 34.1403),
            point!(x: -118.179, y: 34.1403),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, point)| Origin::new(point, 1_775_000_000_000_000 + index as i64 * 5_000_000))
        .collect()
    }

    fn engine(network: MockNetwork) -> Arc<Engine<MockNetwork>> {
        Arc::new(Engine::new(Arc::new(network), (), None))
    }

    /// A minimal identity — the engine only reads its `vehicle_id`, so the rest
    /// is any valid context.
    fn identity() -> JobIdentity {
        JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(7),
            observation: ObservationId {
                partition: 0,
                sequence: 1,
            },
            base: None,
            graph: GraphVersion::new("v1").unwrap(),
            region: RegionId::new("region").unwrap(),
        }
    }

    fn job(context: Continuation<MockEntryId>) -> SolveJob<MockEntryId> {
        SolveJob::new(identity(), Lane::DEFAULT, i64::MAX, context)
    }

    /// A restart over the bend solves to one emitted layer per observation, and
    /// the convergence timestamp — when present — is an observation's own stamp
    /// that the returned trip resumes from.
    #[test]
    fn restart_solves_every_layer() {
        let engine = engine(bent_road());
        let origins = observations();

        let outcome = engine.solve(&job(Continuation::Restart {
            fresh: origins.clone(),
        }));

        let SolveOutcome::Solved {
            diff,
            trip,
            converged_through,
        } = outcome
        else {
            panic!("a restart over an anchored trace must solve, got {outcome:?}");
        };

        assert!(!diff.downgraded, "a fresh restart is never a downgrade");
        assert_eq!(
            diff.layers.len(),
            origins.len(),
            "one emitted layer per observation"
        );

        match converged_through {
            Some(timestamp) => {
                assert!(
                    origins.iter().any(|o| o.timestamp == timestamp),
                    "the convergence stamp is one of the observations'"
                );
                assert_eq!(
                    trip.origins().first().map(|o| o.timestamp),
                    Some(timestamp),
                    "the cut trip resumes from the convergence layer"
                );
            }
            None => assert!(
                !trip.is_empty(),
                "an unfused trip stays whole as the resume state"
            ),
        }
    }

    /// The trip a restart returns resumes on the same engine: pushing one more
    /// on-road observation solves again, without a downgrade.
    #[test]
    fn resume_extends_without_downgrade() {
        let engine = engine(bent_road());

        let SolveOutcome::Solved { trip, .. } = engine.solve(&job(Continuation::Restart {
            fresh: observations(),
        })) else {
            panic!("the seed restart must solve");
        };

        // One more observation on the final edge, stamped after the last.
        let next = Origin::new(
            point!(x: -118.1795, y: 34.1401),
            1_775_000_000_000_000 + 6 * 5_000_000,
        );

        let outcome = engine.solve(&job(Continuation::Resume {
            trip,
            fresh: vec![next],
        }));

        let SolveOutcome::Solved { diff, .. } = outcome else {
            panic!("resuming a supported trip must solve, got {outcome:?}");
        };
        assert!(
            !diff.downgraded,
            "a trip this engine supports resumes rather than downgrades"
        );
    }

    /// A trip solved on a network with a disjoint node-id space is not
    /// supported here: the resume downgrades to a restart over its own
    /// observations and still solves, flagged `downgraded`.
    #[test]
    fn resume_of_foreign_trip_downgrades() {
        let foreign = engine(bent_road_disjoint());
        let SolveOutcome::Solved { trip, .. } = foreign.solve(&job(Continuation::Restart {
            fresh: observations(),
        })) else {
            panic!("the foreign restart must solve");
        };

        let local = engine(bent_road());
        let outcome = local.solve(&job(Continuation::Resume {
            trip,
            fresh: Vec::new(),
        }));

        let SolveOutcome::Solved { diff, .. } = outcome else {
            panic!("a downgraded resume over an anchored trace must solve, got {outcome:?}");
        };
        assert!(
            diff.downgraded,
            "a foreign trip forces the downgrade flag on the emission"
        );
    }

    /// Every observation off every road anchors nothing, so the trip is empty
    /// and the outcome is the nominal [`SolveOutcome::Unanchored`].
    #[test]
    fn all_points_off_network_are_unanchored() {
        let engine = engine(bent_road());

        // The Gulf of Guinea: thousands of kilometres from the LA bend.
        let off: Vec<Origin> = (0..4)
            .map(|i| Origin::new(Point::new(0.0, 0.0), 1_775_000_000_000_000 + i * 5_000_000))
            .collect();

        let outcome = engine.solve(&job(Continuation::Restart { fresh: off }));
        assert!(
            matches!(outcome, SolveOutcome::Unanchored),
            "off-network points solve to Unanchored, got {outcome:?}"
        );
    }

    /// The blocking wrapper returns the same outcome as the direct call.
    #[tokio::test]
    async fn solve_blocking_matches_direct() {
        let engine = engine(bent_road());
        let outcome = engine
            .solve_blocking(job(Continuation::Restart {
                fresh: observations(),
            }))
            .await;
        assert!(
            matches!(outcome, SolveOutcome::Solved { .. }),
            "the blocking solve mirrors the direct one, got {outcome:?}"
        );
    }
}
