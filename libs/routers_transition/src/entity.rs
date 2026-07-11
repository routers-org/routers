use core::marker::PhantomData;

use crate::*;

use crate::definition::{Layer, Layers};
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
/// // The caller owns the match state — the solver grows and fills it, but
/// // never creates it, so it can be inspected, reused, or resumed.
/// let mut state = MatchState::default();
/// let solved = AllComputeSolver::default().solve(transition, &runtime, &mut state)?;
///
/// let interpolated = solved.interpolated(&map);
/// ```
///
/// ### Realtime
///
/// When positions arrive one at a time, build the transition [empty](Self::empty)
/// and [push](Self::push) each point as it lands; every round weighs only the
/// new boundary and resumes the cached forward pass:
///
/// ```ignore
/// let mut transition = Transition::empty(&map, &costing);
/// let mut state = MatchState::default();
///
/// for point in stream {
///     state.extend(transition.push(point, &generator)?)?;
///     let path = solver.solve_path(&transition, &runtime, &mut state)?;
/// }
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

    /// A transition with no points yet: the starting state of a realtime
    /// (streaming) match, grown one position at a time with
    /// [`push`](Transition::push).
    pub fn empty(
        map: &'a N,
        heuristics: &'a CostingStrategies<Emmis, Trans, E>,
    ) -> Transition<'a, Emmis, Trans, E, M, N> {
        Transition {
            map,
            candidates: Candidates::default(),
            layers: Layers::default(),
            heuristics,
            _phantom: PhantomData,
        }
    }

    /// Append one trajectory position as a new layer, returning the layer's
    /// candidate count (its trellis width).
    ///
    /// This is the streaming counterpart of [`new`](Transition::new): a
    /// realtime consumer pushes each point as it arrives, extends its
    /// [`MatchState`](crate::MatchState) by the returned width, and re-solves.
    /// A point with no road candidate within the generator's search radius is
    /// rejected ([`UnanchoredError`]) and leaves the transition unchanged, so
    /// the caller may drop or retry the point.
    pub fn push(
        &mut self,
        origin: geo::Point,
        generator: &impl LayerGeneration<E>,
    ) -> Result<u32, MatchError> {
        let layer = self.layers.layers.len();
        let candidates = generator.candidates(&origin, layer);

        if candidates.is_empty() {
            return Err(UnanchoredError {
                points: vec![Unanchored { layer, origin }],
            }
            .into());
        }

        // Candidate ids are flat insertion order, so appending a layer simply
        // continues the sequence from the current total.
        let mut nodes = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let id = CandidateId::new(self.candidates.lookup.len());
            let _ = self.candidates.lookup.insert(id, candidate);
            nodes.push(id);
        }

        let width = nodes.len() as u32;
        self.layers.layers.push(Layer { nodes, origin });
        Ok(width)
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

    /// The trellis widths for this transition, validated: every input point must
    /// have anchored to at least one candidate.
    pub(crate) fn validated_widths(&self) -> Result<Vec<u32>, MatchError> {
        // Any point with no candidate road within the search radius yields an
        // empty layer and cannot be anchored. Report every such point so the
        // caller can locate all off-network positions at once, not just the
        // first.
        let points = self
            .layers
            .layers
            .iter()
            .enumerate()
            .filter(|(_, layer)| layer.nodes.is_empty())
            .map(|(layer, l)| Unanchored {
                layer,
                origin: l.origin,
            })
            .collect::<Vec<_>>();

        if !points.is_empty() {
            return Err(UnanchoredError { points }.into());
        }

        Ok(self.widths())
    }

    /// Allocate an empty (all-pending) [`Trellis`] sized for this transition's
    /// layers.
    ///
    /// Trellis *construction* is deliberately separated from weight-solving so a
    /// solver only has to fill weights (phase 1) and the graph solve (phase 2) is
    /// left to [`routers_trellis`]. Callers may equally build/inject their own.
    pub fn trellis(&self) -> Result<Trellis, MatchError> {
        // Widths are all non-zero here; an empty trajectory (no layers) falls
        // through to `TrellisError::Empty`, and any other failure is likewise a
        // trellis-level rejection rather than an unanchored/disconnected input.
        Ok(Trellis::new(self.validated_widths()?)?)
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
