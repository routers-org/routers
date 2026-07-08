use std::marker::PhantomData;

use crate::*;

use crate::definition::Layers;
use crate::generation::LayerGeneration;
use geo::LineString;
use routers_network::Network;
use routers_network::{Entry, Metadata};
use routers_trellis::{LayerId, Trellis};

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
/// ```ignore
/// let costing = CostingStrategies::default();
/// let generator = StandardGenerator::new(&map, &costing.emission, DEFAULT_SEARCH_DISTANCE);
/// let transition = Transition::new(&map, route, &costing, generator);
///
/// // The caller owns the trellis — build one (or reuse an existing one) and hand
/// // it to the solver; it is never created inside the solve.
/// let mut trellis = transition.trellis()?;
/// let solved = AllComputeSolver::default().solve(transition, &runtime, &mut trellis)?;
///
/// let interpolated = solved.interpolated(&map);
/// ```
pub struct Transition<'a, Emission, Transition, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Emission: EmissionStrategy,
    Transition: TransitionStrategy<E>,
{
    pub(crate) map: &'a N,
    pub(crate) heuristics: &'a CostingStrategies<Emission, Transition, E>,

    pub(crate) candidates: Candidates<E>,
    pub(crate) layers: Layers,

    _phantom: PhantomData<M>,
}

impl<'a, Emmis, Trans, E, M, N> Transition<'a, Emmis, Trans, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<E> + Send + Sync,
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
        heuristics: &'a CostingStrategies<Emmis, Trans, E>,
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
            _phantom: PhantomData,
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

    /// Per-layer candidate counts — the trellis widths for this transition.
    pub fn widths(&self) -> Vec<u32> {
        self.layers
            .layers
            .iter()
            .map(|layer| layer.nodes.len() as u32)
            .collect()
    }

    /// Allocate an empty (all-pending) [`Trellis`] sized for this transition's
    /// layers.
    ///
    /// Trellis *construction* is deliberately separated from weight-solving so a
    /// solver only has to fill weights (phase 1) and the graph solve (phase 2) is
    /// left to [`routers_trellis`]. Callers may equally build/inject their own.
    pub fn trellis(&self) -> Result<Trellis, MatchError> {
        Trellis::new(self.widths())
            .map_err(|_| MatchError::CollapseFailure(CollapseError::NoPathFound))
    }

    /// The candidate ids of the two layers a [`boundary`](LayerId) joins:
    /// `(from, to)` for the layers on each side of it.
    pub fn boundary(&self, boundary: LayerId) -> (&[CandidateId], &[CandidateId]) {
        let from = boundary.index();
        (
            &self.layers.layers[from].nodes,
            &self.layers.layers[from + 1].nodes,
        )
    }

    /// Map a solved trellis node-path back to the chosen candidate per layer.
    pub fn route_of(&self, path: &routers_trellis::Path) -> Vec<CandidateId> {
        path.nodes
            .iter()
            .enumerate()
            .filter_map(|(layer, node)| {
                self.layers
                    .layers
                    .get(layer)?
                    .nodes
                    .get(node.index())
                    .copied()
            })
            .collect()
    }
}
