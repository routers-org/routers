use crate::graph::Graph;
use crate::transition::*;

use codec::Metadata;
use codec::primitive::Entry;
use geo::LineString;

type LayerId = usize;
type NodeId = usize;

/// A map-specific transition graph based on the Hidden-Markov-Model structure.
///
/// This is the orchestration point for solving transition graphs for making
/// map-matching requests. It requires a [map](Graph) on instantiation, as well as
/// a [route](LineString) to solve for.
///
/// ### Example
///
/// Below is an example that can interpolate a trip using map-matching. To
/// see all the available ways to interpret the resultant solution, see
/// the [`CollapsedPath`] structure.
///
/// ```rust
/// use geo::LineString;
/// use codec::Metadata;
/// use codec::osm::element::Tags;
/// use codec::osm::{OsmEdgeMetadata, OsmEntryId};
/// use routers::{Graph, Transition};
/// use routers::transition::{CostingStrategies, SelectiveForwardSolver};
///
/// // An example function to find the interpolated path of a trip.
/// fn match_trip(map: &Graph<OsmEntryId, OsmEdgeMetadata>, route: LineString) -> Option<LineString> {
///     // Use the default costing strategies
///     let costing = CostingStrategies::default();
///
///     // Create our transition graph, supplying our map for context,
///     // and the route we wish to load as the layer data.
///     let transition = Transition::new(&map, route, costing);
///
///     // For example, let's choose the selective-forward solver.
///     let solver = SelectiveForwardSolver::default();
///
///     // Create our runtime conditions.
///     // These allow us to make on-the-fly changes to costing, such as
///     // our transport mode (Car, Bus, ..) or otherwise.
///     let runtime = OsmEdgeMetadata::runtime();
///
///     // Now.. we simply solve the transition graph using the solver
///     let solution = transition.solve(solver, runtime).ok()?;
///
///     // Then, we can return the interpolated path, just like that!
///     Some(solution.interpolated(map))
/// }
/// ```
pub struct Transition<'a, Emission, Transition, E, M>
where
    E: Entry,
    M: Metadata,
    Emission: EmissionStrategy,
    Transition: TransitionStrategy<E, M>,
{
    pub(crate) map: &'a Graph<E, M>,
    pub(crate) heuristics: CostingStrategies<Emission, Transition, E, M>,

    pub(crate) candidates: Candidates<E>,
    pub(crate) layers: Layers,
}

impl<'a, Emmis, Trans, E, M> Transition<'a, Emmis, Trans, E, M>
where
    E: Entry,
    M: Metadata,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<E, M> + Send + Sync,
{
    /// Creates a new transition graph from the input linestring and heuristics.
    ///
    /// ### Warning
    ///
    /// This function is expensive. Unlike many other `::new(..)` functions, this
    /// function calls out to the [`LayerGenerator`]. This may take significant time
    /// in some circumstances, particularly in longer (>1000 pt) input paths.
    ///
    /// Therefore, this function may be more expensive than intended for some cases,
    /// plan accordingly.
    pub fn new(
        map: &'a Graph<E, M>,
        linestring: LineString,
        heuristics: CostingStrategies<Emmis, Trans, E, M>,
    ) -> Transition<'a, Emmis, Trans, E, M> {
        let points = linestring.into_points();
        let generator = LayerGenerator::new(map, &heuristics);

        // Generate the layers and candidates.
        let (layers, candidates) = generator.with_points(&points);

        Transition {
            map,
            candidates,
            layers,
            heuristics,
        }
    }

    /// Converts the transition graph into a [`RoutingContext`].
    pub fn context<'b>(&'a self, runtime: &'b M::Runtime) -> RoutingContext<'b, E, M>
    where
        'a: 'b,
    {
        RoutingContext {
            candidates: &self.candidates,
            map: self.map,
            runtime,
        }
    }

    /// Solves the transition graph, using the provided [`Solver`].
    pub fn solve(
        self,
        solver: impl Solver<E, M>,
        runtime: &M::Runtime,
    ) -> Result<CollapsedPath<E>, MatchError> {
        // Indirection to call.
        solver.solve(self, runtime)
    }

    /// Collapses the Hidden Markov Model (See [HMM]) into a
    /// [`CollapsedPath`] result (solve).
    ///
    /// Consumes the transition structure in doing so.
    /// This is because it makes irreversible modifications
    /// to the candidate graph that put it in a collapsable
    /// position, and therefore breaks atomicity, and should
    /// not be re-used.
    ///
    /// [HMM]: https://en.wikipedia.org/wiki/Hidden_Markov_model
    pub(crate) fn collapse(self) -> Result<CollapsedPath<E>, MatchError> {
        // Use the candidates to collapse the graph into a single route.
        self.candidates
            .collapse()
            .map_err(MatchError::CollapseFailure)
    }
}
