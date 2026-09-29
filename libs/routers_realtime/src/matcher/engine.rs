//! Matcher: the solve engine.
//!
//! [`Engine`] is the map-matching core. It owns the heavy, shareable state a
//! solve reaches into — the loaded network, its runtime, the costing strategies
//! and the reachability cache — so a pool of blocking solves can fan out over
//! one `Arc<Engine>`. It is generic over the [`Network`] so tests drive it
//! against a mock. A solve returns a typed [`SolveOutcome`]; the engine never
//! rejects a job, it only solves the one it is handed.

use alloc::sync::Arc;
use core::future::Future;
use core::time::Duration;

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
use crate::protocol::job::{SolveJob, TripDigest};
use crate::protocol::result::SolveOutcome;

/// Timing of one blocking-pool solve, separated from the time it waited for a worker.
pub struct BlockingSolve<E: Entry> {
    /// The solve's typed answer.
    pub outcome: SolveOutcome<E>,
    /// Time from submission until a blocking worker began executing the job.
    pub queued_for: Duration,
    /// Time spent executing the algorithm on that worker.
    pub ran_for: Duration,
}

/// The shared, reusable state one region's matcher solves against.
pub struct Engine<N: Network> {
    network: Arc<N>,
    runtime: N::Runtime,
    costing: CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, N::Entry>,
    cache: Arc<PredicateCache<N>>,
    search_distance: Option<f64>,
    max_candidates: Option<usize>,
    window_layers: Option<usize>,
}

impl<N: Network> Engine<N> {
    /// Build an engine over a loaded `network` and its `runtime`, using the
    /// default costing strategies and a fresh reachability cache.
    ///
    /// `search_distance` overrides the candidate generator's default reach when
    /// `Some`; `None` keeps the generator's own default.
    pub fn new(network: Arc<N>, runtime: N::Runtime, search_distance: Option<f64>) -> Self {
        Self {
            network,
            runtime,
            costing: CostingStrategies::default(),
            cache: Arc::new(PredicateCache::default()),
            search_distance,
            max_candidates: None,
            window_layers: None,
        }
    }

    /// Keep only the best `max_candidates` per observation; `None` keeps every edge in range.
    #[must_use]
    pub fn with_max_candidates(mut self, max_candidates: Option<usize>) -> Self {
        self.max_candidates = max_candidates;
        self
    }

    /// Carry at most `window_layers` between solves; older layers are finalised at the cut.
    #[must_use]
    pub fn with_window_layers(mut self, window_layers: Option<usize>) -> Self {
        self.window_layers = window_layers;
        self
    }

    /// Bound the reachability and nested successor caches independently.
    ///
    /// # Panics
    ///
    /// Panics if either bound is zero or not a power of two.
    #[must_use]
    pub fn with_cache_capacities(mut self, predicates: usize, successors: usize) -> Self {
        self.cache = Arc::new(PredicateCache::with_capacities(predicates, successors));
        self
    }

    /// Solve one job, returning the typed outcome the owner commits against.
    ///
    /// A [`Continuation::Resume`] whose trip references edges this network does
    /// not serve is downgraded to a restart over the trip's own observations,
    /// with `diff.downgraded` set. Fresh observations that anchor to no road are
    /// dropped; a solve left with nothing to match is [`SolveOutcome::Unanchored`].
    pub fn solve(&self, job: &SolveJob<N::Entry>) -> SolveOutcome<N::Entry> {
        let vehicle_id = job.identity.vehicle_id;

        let mut generator = StandardGenerator::new(self.network.as_ref(), &self.costing.emission);
        if let Some(distance) = self.search_distance {
            generator = generator.with_search_distance(distance);
        }
        generator = generator.with_max_candidates(self.max_candidates);

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
            window_cut = field::Empty,
            emitted = field::Empty,
        );
        let _entered = span.enter();

        let (mut trip, fresh, downgraded) = match job.context.clone() {
            // A resume referencing edges this shard does not serve is degraded to
            // a restart over the trip's own observations, healing the seam under
            // a higher revision.
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
            debug!("{vehicle_id}: no anchored layers to solve");
            return SolveOutcome::Unanchored;
        }

        if let Err(err) = info_span!("solve").in_scope(|| matcher.solve(&mut trip)) {
            let (outcome, severity) = classify(&err);
            span.record("outcome", outcome);
            span.record("severity", severity);
            if severity == "nominal" {
                debug!("{vehicle_id}: unable to solve trip: {err}");
            } else {
                error!("{vehicle_id}: unable to solve trip: {err}");
            }
            return terminal_outcome(err);
        }

        // Copied out: the snapshot borrows the whole trip mutably.
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

        // Revision 0 is a placeholder: the owner stamps the real revision (the
        // raw stream sequence) before publishing.
        let mut diff = info_span!("emit")
            .in_scope(|| MatchedDiff::new(&solution, &origins, self.network.as_ref(), 0));
        diff.downgraded = downgraded;
        drop(solution);
        span.record("emitted", diff.layers.len());

        // Read the convergence timestamp from `origins` before the cut: tailing
        // renumbers the trip's layers, but the copied origins still index by the
        // pre-cut layer.
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

        // The window cap finalises what convergence did not.
        let converged_through = match self.window_layers {
            Some(window) if trip.layers() > window => {
                let cut = trip.origins()[trip.layers() - window - 1].timestamp;
                trip.tail(window);
                span.record("window_cut", true);
                Some(converged_through.map_or(cut, |c| c.max(cut)))
            }
            _ => converged_through,
        };

        span.record("outcome", "success");
        span.record("severity", "ok");

        // Digested here, on the blocking solve thread, once for both the
        // trip cache and the owner's next job id.
        let trip_digest = Some(TripDigest::of(&trip));
        SolveOutcome::Solved {
            diff,
            trip,
            converged_through,
            trip_digest,
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
    /// A panic inside the solve is caught and mapped to [`SolveOutcome::Internal`]
    /// so one poisoned job never takes the pull loop down with it.
    pub fn solve_blocking(
        self: &Arc<Self>,
        job: SolveJob<N::Entry>,
    ) -> impl Future<Output = SolveOutcome<N::Entry>> {
        let engine = Arc::clone(self);
        async move { engine.solve_blocking_timed(job).await.outcome }
    }

    /// Solve on the blocking pool and separate worker-queue delay from execution.
    #[expect(
        clippy::disallowed_methods,
        reason = "web_time reexports std::time::Instant on native targets"
    )]
    pub async fn solve_blocking_timed(
        self: &Arc<Self>,
        job: SolveJob<N::Entry>,
    ) -> BlockingSolve<N::Entry> {
        let engine = Arc::clone(self);
        let queued_at = web_time::Instant::now();
        tokio::task::spawn_blocking(move || {
            let started_at = web_time::Instant::now();
            let outcome = engine.solve(&job);
            BlockingSolve {
                outcome,
                queued_for: started_at.duration_since(queued_at),
                ran_for: started_at.elapsed(),
            }
        })
        .await
        .unwrap_or_else(|err| {
            error!("solve task panicked: {err}");
            BlockingSolve {
                outcome: SolveOutcome::Internal {
                    reason: "panic".to_owned(),
                },
                queued_for: queued_at.elapsed(),
                ran_for: Duration::ZERO,
            }
        })
    }
}

/// A match attempt's `outcome`/`severity` span labels for the success-ratio
/// series. Nominal failures are the data's fault; fatal ones are ours.
fn classify(err: &MatchError) -> (&'static str, &'static str) {
    match err {
        MatchError::Unanchored(_) => ("unanchored", "nominal"),
        MatchError::Disconnected(_) => ("disconnected", "nominal"),
        MatchError::TrellisError(_) | MatchError::SolveError(_) => ("internal", "fatal"),
    }
}

/// Project a solve failure onto its terminal [`SolveOutcome`]. A trellis or
/// solver error carries its message for the logs, never a label.
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
    use routers_transition::matcher::Trip;

    use crate::event::VehicleId;
    use crate::protocol::ids::{GraphVersion, Lane, ObservationId, RegionId, SCHEMA_VERSION};
    use crate::protocol::job::{JobIdentity, SolveJob};

    /// A staircase road (west, south, west) — the shared `matched_diff` shape.
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

    /// The same road geometry with a disjoint node-id space, so a trip solved
    /// here fails `matcher.supports` on a `bent_road` engine and forces a downgrade.
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

    /// Six spaced observations tracing the bend, five seconds apart.
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

    /// A minimal identity; the engine only reads its `vehicle_id`.
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

    /// Solve an incremental chain over `origins` (restart, then resume from
    /// each result's trip), returning every outcome's bytes.
    fn chain(engine: &Engine<MockNetwork>, origins: &[Origin]) -> Vec<Vec<u8>> {
        let mut trip: Option<Trip<MockEntryId>> = None;
        let mut bytes = Vec::new();
        for &origin in origins {
            let context = match trip.take() {
                Some(trip) => Continuation::Resume {
                    trip,
                    fresh: vec![origin],
                },
                None => Continuation::Restart {
                    fresh: vec![origin],
                },
            };
            let outcome = engine.solve(&job(context));
            bytes.push(postcard::to_allocvec(&outcome).unwrap());
            if let SolveOutcome::Solved { trip: next, .. } = outcome {
                trip = Some(next);
            }
        }
        bytes
    }

    #[test]
    fn solves_do_not_depend_on_the_engines_history() {
        let origins = observations();
        let reference = chain(&engine(bent_road()), &origins);

        // Tiny caches that evict constantly, warmed by unrelated work first.
        let thrashing = Engine::new(Arc::new(bent_road()), (), None).with_cache_capacities(64, 64);
        for shift in 0..8 {
            let mut shuffled = origins.clone();
            shuffled.rotate_left(shift % origins.len());
            let _ = chain(&thrashing, &shuffled);
        }
        assert_eq!(chain(&thrashing, &origins), reference);

        // The same engine re-solving the chain agrees with its first run.
        let warm = engine(bent_road());
        assert_eq!(chain(&warm, &origins), reference);
        assert_eq!(chain(&warm, &origins), reference);
    }

    fn job(context: Continuation<MockEntryId>) -> SolveJob<MockEntryId> {
        SolveJob::new(identity(), Lane::DEFAULT, i64::MAX, context)
    }

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
            trip_digest,
        } = outcome
        else {
            panic!("a restart over an anchored trace must solve, got {outcome:?}");
        };

        assert_eq!(
            trip_digest,
            Some(TripDigest::of(&trip)),
            "the engine digests its trip"
        );
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

    #[test]
    fn resume_extends_without_downgrade() {
        let engine = engine(bent_road());

        let SolveOutcome::Solved { trip, .. } = engine.solve(&job(Continuation::Restart {
            fresh: observations(),
        })) else {
            panic!("the seed restart must solve");
        };

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

    #[test]
    fn all_points_off_network_are_unanchored() {
        let engine = engine(bent_road());

        // The Gulf of Guinea: far from the LA bend.
        let off: Vec<Origin> = (0..4)
            .map(|i| Origin::new(Point::new(0.0, 0.0), 1_775_000_000_000_000 + i * 5_000_000))
            .collect();

        let outcome = engine.solve(&job(Continuation::Restart { fresh: off }));
        assert!(
            matches!(outcome, SolveOutcome::Unanchored),
            "off-network points solve to Unanchored, got {outcome:?}"
        );
    }

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

    #[tokio::test]
    async fn timed_blocking_solve_reports_execution() {
        let engine = engine(bent_road());
        let solved = engine
            .solve_blocking_timed(job(Continuation::Restart {
                fresh: observations(),
            }))
            .await;

        assert!(matches!(solved.outcome, SolveOutcome::Solved { .. }));
        assert!(solved.ran_for > Duration::ZERO);
    }
}
