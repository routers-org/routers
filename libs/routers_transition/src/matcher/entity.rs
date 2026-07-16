use core::marker::PhantomData;

use geo::{LineString, Point};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::{LayerId, Path, SolveError, TrellisError, ViterbiSolver};

use crate::costing::{EmissionStrategy, TransitionStrategy};
use crate::generation::LayerGeneration;
use crate::matcher::trip::TripState;
use crate::weigh::frontier_collapse;
use crate::{
    CandidateRef, CollapsedPath, CostingStrategies, Disconnected, DisconnectedError, MatchError,
    Reachable, RoutingContext, Trip, Unanchored, UnanchoredError, Weigher,
};

/// The map-matching orchestrator: configuration and borrowed context only.
///
/// All mutable state lives in the caller-owned [`Trip`]; a `Matcher` is the
/// set of operations over it. Two shapes of use:
///
/// **Batch** — one call:
///
/// ```ignore
/// let costing = CostingStrategies::default();
/// let generator = StandardGenerator::new(&map, &costing.emission, DEFAULT_SEARCH_DISTANCE);
/// let matcher = Matcher::new(&map, &costing, generator, AllCompute::default(), &runtime);
///
/// let collapsed = matcher.r#match(linestring)?;
/// ```
///
/// **Streaming** — positions arrive one at a time; the caller owns the trip
/// and hands it back each tick. Solving is defined as weigh-then-solve, so
/// there is no weighing step to forget:
///
/// ```ignore
/// let mut trip = matcher.begin();
/// for point in stream {
///     matcher.push(&mut trip, point)?;
///     let path = matcher.solve(&mut trip)?;
/// }
/// let collapsed = matcher.finish(trip)?;
/// ```
pub struct Matcher<'a, Emmis, Trans, G, W, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<E> + Send + Sync,
    G: LayerGeneration<E>,
    W: Weigher<E, M, N> + Sync,
{
    map: &'a N,
    heuristics: &'a CostingStrategies<Emmis, Trans, E>,
    generator: G,
    weigher: W,
    runtime: &'a M::Runtime,

    _phantom: PhantomData<(E, M)>,
}

impl<'a, Emmis, Trans, G, W, E, M, N> Matcher<'a, Emmis, Trans, G, W, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<E> + Send + Sync,
    G: LayerGeneration<E>,
    W: Weigher<E, M, N> + Sync,
{
    pub fn new(
        map: &'a N,
        heuristics: &'a CostingStrategies<Emmis, Trans, E>,
        generator: G,
        weigher: W,
        runtime: &'a M::Runtime,
    ) -> Self {
        Self {
            map,
            heuristics,
            generator,
            weigher,
            runtime,
            _phantom: PhantomData,
        }
    }

    /// A fresh, empty [`Trip`].
    pub fn begin(&self) -> Trip<E> {
        Trip::new()
    }

    /// The [`RoutingContext`] over a trip's candidates.
    fn context<'b>(&'b self, trip: &'b Trip<E>) -> RoutingContext<'b, E, M, N> {
        RoutingContext {
            candidates: trip.candidates(),
            map: self.map,
            runtime: self.runtime,
        }
    }

    /// Append one trajectory position as a new layer: generate its candidates,
    /// extend the trellis, and record the emission costs as node weights — one
    /// atomic operation, so the trip cannot desynchronise.
    ///
    /// A point with no road candidate within the generator's search radius is
    /// rejected ([`UnanchoredError`]) and leaves the trip unchanged, so the
    /// caller may drop or retry the point.
    pub fn push(&self, trip: &mut Trip<E>, origin: Point) -> Result<LayerId, MatchError> {
        let layer = trip.layers();
        let candidates = self.generator.candidates(&origin, layer);

        if candidates.is_empty() {
            return Err(UnanchoredError {
                points: vec![Unanchored { layer, origin }],
            }
            .into());
        }

        Ok(trip.push_layer(origin, candidates)?)
    }

    /// Append many positions at once, generating their candidates in parallel.
    ///
    /// Any point with no candidate rejects the whole batch ([`UnanchoredError`]
    /// reporting *every* such point) and leaves the trip unchanged.
    pub fn extend(&self, trip: &mut Trip<E>, points: &[Point]) -> Result<(), MatchError> {
        let first_layer = trip.layers();
        let per_layer = self.generator.generate(points, first_layer);

        let unanchored = per_layer
            .iter()
            .zip(points)
            .enumerate()
            .filter(|(_, (candidates, _))| candidates.is_empty())
            .map(|(offset, (_, &origin))| Unanchored {
                layer: first_layer + offset,
                origin,
            })
            .collect::<Vec<_>>();
        if !unanchored.is_empty() {
            return Err(UnanchoredError { points: unanchored }.into());
        }

        for (&origin, candidates) in points.iter().zip(per_layer) {
            trip.push_layer(origin, candidates)?;
        }
        Ok(())
    }

    /// Weigh every pending boundary and find the minimum-cost path through the
    /// trip.
    ///
    /// Already-resolved boundaries are never recomputed, so re-solving after an
    /// append weighs only the new boundaries (plus a µs-scale full DP pass).
    pub fn solve<'b>(&self, trip: &'b mut Trip<E>) -> Result<&'b Path, MatchError> {
        let mut trellis = match trip.take_state() {
            TripState::Empty => return Err(TrellisError::Empty.into()),
            TripState::Solved(solved) => {
                // Nothing pending: the certificate stands.
                trip.restore(TripState::Solved(solved));
                return Ok(trip.path().expect("solved trip has a path"));
            }
            TripState::Building(trellis) => trellis,
        };

        let weighed = {
            let ctx = self.context(trip);
            self.weigher.weigh(&ctx, self.heuristics, &mut trellis)
        };
        if let Err(e) = weighed {
            trip.restore(TripState::Building(trellis));
            return Err(e);
        }

        // Gaps: boundaries the weigher left pending because nothing bridged them.
        let gaps = trellis.disconnections();
        if !gaps.is_empty() {
            let error = self.disconnected(trip, gaps);
            trip.restore(TripState::Building(trellis));
            return Err(error);
        }

        match trellis.solve(&ViterbiSolver::new()) {
            Ok(solved) => {
                trip.restore(TripState::Solved(solved));
                Ok(trip.path().expect("solved trip has a path"))
            }
            Err((trellis, SolveError::Unreachable)) => {
                // Every boundary resolved, yet the reachable frontier dies
                // mid-way: some boundary has edges but none continue a live
                // path. Walk the resolved weights to find where.
                let error = self.disconnected(trip, frontier_collapse(&trellis));
                trip.restore(TripState::Building(trellis));
                Err(error)
            }
            Err((trellis, e)) => {
                trip.restore(TripState::Building(trellis));
                Err(e.into())
            }
        }
    }

    /// Solve (if pending) and collapse the trip into its final match,
    /// re-deriving each chosen hop's routed geometry from the (warm) predicate
    /// cache — nothing is stored during weighing.
    pub fn finish(&self, mut trip: Trip<E>) -> Result<CollapsedPath<E>, MatchError> {
        let path = self.solve(&mut trip)?;
        let cost = path.cost;

        let route = self.route_of(path);
        let interpolated = {
            let ctx = self.context(&trip);
            route
                .windows(2)
                .filter_map(|hop| match hop {
                    [from, to] => self.weigher.reach(&ctx, *from, *to),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        let (candidates, _) = trip.into_parts();

        Ok(CollapsedPath {
            cost,
            route,
            interpolated,
            candidates,
        })
    }

    /// Match a whole trajectory in one call: batch candidate generation,
    /// parallel weighing, solve, and collapse.
    pub fn r#match(&self, linestring: LineString) -> Result<CollapsedPath<E>, MatchError> {
        let mut trip = self.begin();
        self.extend(&mut trip, &linestring.into_points())?;
        self.finish(trip)
    }

    /// Re-derive the routed geometry of a single hop — for realtime consumers
    /// that want interpolated output per tick. Cache-warm and deterministic;
    /// the caller owns any per-tick memoisation.
    pub fn hop(
        &self,
        trip: &Trip<E>,
        from: CandidateRef,
        to: CandidateRef,
    ) -> Option<Reachable<E>> {
        let ctx = self.context(trip);
        self.weigher.reach(&ctx, from, to)
    }

    /// Map a solved node-path to the chosen candidate per layer.
    fn route_of(&self, path: &Path) -> Vec<CandidateRef> {
        path.nodes
            .iter()
            .enumerate()
            .map(|(layer, &node)| CandidateRef::new(LayerId(layer as u32), node))
            .collect()
    }

    /// Boundary breaks as a [`DisconnectedError`], carrying the origins of the
    /// layers on each side.
    fn disconnected(&self, trip: &Trip<E>, breaks: Vec<LayerId>) -> MatchError {
        let breaks = breaks
            .into_iter()
            .map(|boundary| {
                let (from, to) = (boundary.index(), boundary.index() + 1);
                Disconnected {
                    from_layer: from,
                    to_layer: to,
                    from_origin: trip.point(boundary).unwrap_or_default(),
                    to_origin: trip.point(LayerId(to as u32)).unwrap_or_default(),
                }
            })
            .collect::<Vec<_>>();
        DisconnectedError { breaks }.into()
    }
}
