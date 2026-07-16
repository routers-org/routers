use alloc::borrow::Cow;
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

/// For orchestrating a map match, use the [`Matcher`] struct.
///
/// A matcher requires a set of costing strategies, which define how to determine the emission
/// and transition costs of a solve, alongside a solver strategy. By default, the [`Matcher`]
/// uses the [`StandardGenerator`] to generate candidates, and the [`AllCompute`] strategy to
/// solve the transition graph.
///
/// ```ignore
/// let costing = CostingStrategies::default();
/// let generator = StandardGenerator::new(&map, &costing.emission, DEFAULT_SEARCH_DISTANCE);
/// let matcher = Matcher::new(&map, &costing, generator, AllCompute::default(), &runtime);
/// ```
///
/// A runtime and map are also required to solve a map matching problem. The runtime will come
/// from your map implementation, and the map will be provided by the caller. These are your
/// network abstractions build in `routers_network`.
///
/// There are two primary methods to use a [`Matcher`]:
///
/// ## Batch matching
///
/// This is where you have the entire linestring available upfront, and the matcher will solve
/// the transition graph in one call. This is the fastest approach, and has the least overhead.
///
/// ```ignore
/// // Simply solve using the `r#match(..)` method.
/// let solution = matcher.r#match(linestring)?;
/// ```
///
/// ## Stream matching
///
/// This is the approach you should take should positions arrive one at a time. This is a more
/// involved method as it requires owning the state between calls.
///
/// ```ignore
/// // Initialize the trip state using `begin(..)`, this gives you a [`Trip`] state.
/// let mut trip = matcher.begin();
///
/// // In your iteration over inbound streams, i.e. an iterator, push positions onto
/// // the trip state using `push(..)`, and then solve using `solve(..)`.
/// for point in stream {
///     matcher.push(&mut trip, point)?;
///     let path = matcher.solve(&mut trip)?;
///
///     // This is the solved path for the current position, which only contains
///     // the cost and route, not the interpolated points. This is cheaper, and preferred
///     // within a hot-loop.
/// }
///
/// // So, if you do require an interpolated route or some further map-based context,
/// // given any solved point, you can obtain this information by using `snapshot(..)`.
/// let collapsed = matcher.snapshot(&mut trip)?;
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

/// The store-independent parts of a [`CollapsedPath`], as derived by
/// [`Matcher::collapse`].
struct Collapse<E>
where
    E: Entry,
{
    cost: u32,
    route: Vec<CandidateRef>,
    interpolated: Vec<Reachable<E>>,
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
        let layer = trip.next_id();
        let candidates = self.generator.candidates(&origin, layer);

        if candidates.is_empty() {
            return Err(UnanchoredError {
                points: vec![Unanchored {
                    layer: layer.index(),
                    origin,
                }],
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
        let first_layer = trip.next_id();
        let per_layer = self.generator.generate(points, first_layer);

        let unanchored = per_layer
            .iter()
            .zip(points)
            .enumerate()
            .filter(|(_, (candidates, _))| candidates.is_empty())
            .map(|(offset, (_, &origin))| Unanchored {
                layer: first_layer.index() + offset,
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

    /// Solve (if pending) and collapse the trip's current solution into a
    /// [`CollapsedPath`], re-deriving each chosen hop's routed geometry from
    /// the (warm) predicate cache — nothing is stored during weighing.
    ///
    /// The trip is not consumed: the snapshot borrows its candidates, so the
    /// caller may keep streaming once the snapshot is dropped (or detached
    /// with [`CollapsedPath::into_owned`]).
    pub fn snapshot<'t>(&self, trip: &'t mut Trip<E>) -> Result<CollapsedPath<'t, E>, MatchError> {
        let Collapse {
            cost,
            route,
            interpolated,
        } = self.collapse(trip)?;

        Ok(CollapsedPath {
            cost,
            route,
            interpolated,
            candidates: Cow::Borrowed(trip.candidates()),
        })
    }

    /// Match a whole trajectory in one call: batch candidate generation,
    /// parallel weighing, solve, and collapse. The trip is internal here, so
    /// the result owns its candidates.
    pub fn r#match(&self, linestring: LineString) -> Result<CollapsedPath<'a, E>, MatchError> {
        let mut trip = self.begin();
        self.extend(&mut trip, &linestring.into_points())?;

        let Collapse {
            cost,
            route,
            interpolated,
        } = self.collapse(&mut trip)?;
        let (candidates, _) = trip.into_parts();

        Ok(CollapsedPath {
            cost,
            route,
            interpolated,
            candidates: Cow::Owned(candidates),
        })
    }

    /// Solve (if pending) and derive the collapse: total cost, the chosen
    /// candidate per layer, and each hop's routed geometry.
    fn collapse(&self, trip: &mut Trip<E>) -> Result<Collapse<E>, MatchError> {
        let path = self.solve(trip)?;
        let cost = path.cost;

        let route = self.route_of(path);
        let interpolated = {
            let ctx = self.context(trip);
            route
                .windows(2)
                .filter_map(|hop| match hop {
                    [from, to] => self.weigher.reach(&ctx, *from, *to),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        Ok(Collapse {
            cost,
            route,
            interpolated,
        })
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
