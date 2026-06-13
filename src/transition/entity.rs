use crate::transition::*;

use crate::definition::Layers;
use crate::generation::LayerGeneration;
use geo::LineString;
use routers_network::Network;
use routers_network::{Entry, Metadata};

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
/// use routers_codec::Metadata;
/// use routers_codec::osm::element::Tags;
/// use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
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
pub struct Transition<'a, Emission, Transition, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Emission: EmissionStrategy,
    Transition: TransitionStrategy<E, M, N>,
{
    pub(crate) map: &'a N,
    pub(crate) heuristics: &'a CostingStrategies<Emission, Transition, E, M, N>,

    pub(crate) candidates: Candidates<E>,
    pub(crate) layers: Layers,
}

impl<'a, Emmis, Trans, E, M, N> Transition<'a, Emmis, Trans, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<E, M, N> + Send + Sync,
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
        map: &'a N,
        linestring: LineString,
        heuristics: &'a CostingStrategies<Emmis, Trans, E, M, N>,
        generator: impl LayerGeneration<E>,
    ) -> Transition<'a, Emmis, Trans, E, M, N> {
        let points = linestring.into_points();

        // Generate the layers and candidates.
        let (layers, candidates) = generator.generate(&points);

        Transition {
            map,
            candidates,
            layers,
            heuristics,
        }
    }

    /// Constructs a transition graph from pre-built [`Layers`] and
    /// [`Candidates`], bypassing the [`LayerGenerator`].
    ///
    /// Use this when the candidate set is not derived from a single
    /// linestring — for example, when extending a saved Viterbi
    /// frontier by one new GPS point, where L0 candidates come from
    /// the prior frontier rather than from candidate generation.
    ///
    /// The caller is responsible for ensuring `layers` and
    /// `candidates` are mutually consistent: every [`CandidateId`] in
    /// `layers.layers[*].nodes` must resolve in `candidates.lookup`.
    pub fn from_parts(
        map: &'a N,
        heuristics: &'a CostingStrategies<Emmis, Trans, E, M, N>,
        layers: Layers,
        candidates: Candidates<E>,
    ) -> Transition<'a, Emmis, Trans, E, M, N> {
        Transition {
            map,
            candidates,
            layers,
            heuristics,
        }
    }


    /// Converts the transition graph into a [`RoutingContext`].
    pub fn context<'b>(&'a self, runtime: &'b M::Runtime) -> RoutingContext<'b, E, M, N>
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
        solver: impl Solver<E, M, N>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::costing::CostingStrategies;
    use crate::generation::StandardGenerator;
    use crate::r#match::DEFAULT_SEARCH_DISTANCE;
    use crate::solver::PrecomputeForwardSolver;
    use crate::testing::{MockEntryId, MockMetadata, MockNetwork, MockNetworkBuilder};
    use geo::{point, wkt};

    fn straight_road() -> MockNetwork {
        MockNetworkBuilder::new()
            .node(1, point!(x: -118.14, y: 34.15))
            .node(2, point!(x: -118.15, y: 34.15))
            .node(3, point!(x: -118.16, y: 34.15))
            .node(4, point!(x: -118.17, y: 34.15))
            .edge(1, 2)
            .edge(2, 3)
            .edge(3, 4)
            .build()
    }

    /// `from_parts` over generator outputs must produce the same solve
    /// result as the equivalent `Transition::new` call. This pins the
    /// contract that the two constructors are interchangeable when the
    /// `Layers` + `Candidates` arguments come from the same generator.
    #[test]
    fn from_parts_solves_equivalently_to_new() {
        let net = straight_road();
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.141 34.1503,
                -118.155 34.1503,
                -118.169 34.1503
            )
        };
        let costing = CostingStrategies::default();

        // Reference: build via `Transition::new`.
        let gen_a = StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
        let trans_a = Transition::new(&net, linestring.clone(), &costing, gen_a);
        let solver_a = PrecomputeForwardSolver::<MockEntryId, MockMetadata, MockNetwork>::default();
        let result_a = solver_a.solve(trans_a, &()).expect("baseline solve succeeds");

        // Under test: invoke the generator manually, then `from_parts`.
        let gen_b = StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
        let points = linestring.into_points();
        let (layers, candidates) = gen_b.generate(&points);
        let trans_b = Transition::from_parts(&net, &costing, layers, candidates);
        let solver_b = PrecomputeForwardSolver::<MockEntryId, MockMetadata, MockNetwork>::default();
        let result_b = solver_b.solve(trans_b, &()).expect("from_parts solve succeeds");

        assert_eq!(result_a.cost, result_b.cost);
        assert_eq!(result_a.route, result_b.route);
    }
}
