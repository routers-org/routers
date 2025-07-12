use crate::graph::Graph;
use crate::transition::*;

use geo::{Distance, Haversine, LineString};
use itertools::Itertools;
use pathfinding::num_traits::ConstZero;
use routers_codec::Metadata;
use routers_codec::primitive::Entry;

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
///     let runtime = OsmEdgeMetadata::runtime(None);
///
///     // Now.. we simply solve the transition graph using the solver
///     let solution = transition.solve(solver, &runtime).ok()?;
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
    ///
    ///
    ///       /// Collapses transition layers, `layers`, into a single vector of
    //         /// the finalised points. This is useful for solvers which will
    //         /// mutate the candidates, and require an external method to solve
    //         /// based on the calculated edge weights. Iterative solvers which
    //         /// do not produce a candidate solution do not require this function.
    //         ///
    //         /// Takes an owned value to indicate the structure is [terminal].
    //         ///
    //         /// [terminal]: Cannot be used again
    pub(crate) fn collapse(
        self,
        lookup: &scc::HashMap<(usize, usize), Reachable<E>>,
    ) -> Result<CollapsedPath<E>, MatchError> {
        // There should be exclusive read-access by the time collapse is called.
        // This will block access to any other client using this candidate structure,
        // however this function
        let graph = self
            .candidates
            .graph
            .read()
            .map_err(|_| MatchError::CollapseFailure(CollapseError::ReadLockFailed))?;

        // Calculates the combination of emission and transition costs.
        let cost_fn = |target: &CandidateRef, edge: &CandidateEdge| {
            // Decaying Transition Cost
            let transition_cost = edge.weight;

            // Loosely-Decaying Emission Cost
            let emission_cost = target.weight();

            let transition = (transition_cost as f64 * 0.6) as u32;
            let emission = (emission_cost as f64 * 0.4) as u32;

            emission.saturating_add(transition)
        };

        let successors = |candidate: &CandidateId| {
            let next = self.candidates.next_layer(candidate);

            next.into_iter()
                .filter_map(|next_candidate| {
                    Some((
                        next_candidate,
                        self.candidates.edge(candidate, &next_candidate)?,
                    ))
                })
                .collect::<Vec<_>>()
        };

        let bridge = Bridge::new(self.candidates.source, self.candidates.target)
            .layered(self.layers.first().unwrap(), self.layers.last().unwrap());

        let Some((route, cost)) = pathfinding::directed::astar::astar(
            &self.candidates.source,
            |node| {
                bridge
                    .handle(node, successors)
                    .into_iter()
                    .filter_map(|(candidate, cost)| {
                        Some((candidate, cost_fn(graph.node_weight(candidate)?, &cost)))
                    })
            },
            |_| u32::ZERO,
            |&node| self.candidates.target == node,
        ) else {
            return Err(MatchError::CollapseFailure(CollapseError::NoPathFound));
        };

        drop(graph);

        let reached = route
            .windows(2)
            .filter_map(|nodes| {
                if let [a, b] = nodes {
                    lookup
                        .get(&(a.index(), b.index()))
                        .map(|entry| entry.get().clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        Ok(CollapsedPath::new(cost, reached, route, self.candidates))
    }

    /// TODO: Docs
    /// Resolves the transition cost for a reachable element in the transition
    /// graph given some transition context.
    pub(crate) fn resolve(
        &self,
        context: &RoutingContext<E, M>,
        reachable: Reachable<E>,
    ) -> Option<(Reachable<E>, CandidateEdge)> {
        let path_vec = reachable.path_nodes().collect_vec();

        let optimal_path = Trip::new_with_map(self.map, &path_vec);

        let source = context.candidate(&reachable.source)?;
        let target = context.candidate(&reachable.target)?;

        let sl = self.layers.layers.get(source.location.layer_id)?;
        let tl = self.layers.layers.get(target.location.layer_id)?;

        let layer_width = Haversine.distance(sl.origin, tl.origin);

        let transition_cost = self.heuristics.transition(TransitionContext {
            map_path: &path_vec,
            requested_resolution_method: reachable.resolution_method,

            source_candidate: &reachable.source,
            target_candidate: &reachable.target,
            routing_context: &context,

            layer_width,
            optimal_path,
        });

        Some((reachable, CandidateEdge::new(transition_cost)))
    }
}
