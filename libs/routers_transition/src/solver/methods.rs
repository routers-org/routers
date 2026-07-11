use crate::{
    Candidate, CandidateId, CollapsedPath, Costing, Disconnected, DisconnectedError, MatchError,
    PredicateCache, Reachable, RoutingContext, SideTable, Transition, TransitionContext,
    costing::{EmissionStrategy, TransitionStrategy},
    solver::expansion::Expansion,
};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::{LayerId, MAX_WEIGHT, NO_EDGE, NodeId, Solve, Trellis, ViterbiSolver};

use itertools::Itertools;
use rayon::prelude::*;

/// The hop side-data produced while weighing a boundary: each routed
/// [`Reachable`] keyed by its `(from, to)` candidate pair, ready to extend a
/// [`SideTable`].
type Hops<E> = Vec<((CandidateId, CandidateId), Reachable<E>)>;

/// A strategy for collapsing a [`Transition`] into a matched [`CollapsedPath`].
///
/// Every solver fills a caller-owned [`Trellis`] with transition weights and lets
/// `routers_trellis` find the minimum-cost path. A strategy supplies only two
/// things — its [`cache`](Solver::cache), and [which next-layer candidates to
/// weigh](Solver::select) for a source — and inherits the whole pipeline
/// (`hop` → `weigh_source` → `weigh_boundary` → `weigh` → `solve`).
///
/// Weighing touches only **pending** boundaries, so handing in a partially-solved
/// trellis resumes it rather than redoing the solved parts.
pub trait Solver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    /// The predicate cache backing this solver's reachability queries.
    fn cache(&self) -> &PredicateCache<E, M, N>;

    /// Which next-layer candidates to weigh for `source`, as positions within
    /// `to_layer`. All-compute returns all of them; a selective strategy returns a
    /// promising subset.
    fn select(
        &self,
        ctx: &RoutingContext<E, M, N>,
        source: &Candidate<E>,
        to_layer: &[CandidateId],
    ) -> Vec<NodeId>;

    /// The cost and routed path of the transition `from -> to`, or `None` when
    /// `to` is unreachable. `source_emission` is folded in only for first-layer
    /// sources; the cost is clamped to the trellis weight ceiling.
    fn hop<Emmis, Trans>(
        &self,
        ctx: &RoutingContext<E, M, N>,
        transition: &Transition<Emmis, Trans, E, M, N>,
        from: CandidateId,
        to: CandidateId,
        source_emission: u32,
    ) -> Option<(u32, Reachable<E>)>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
    {
        let reachable = Expansion::new(ctx, self.cache()).reach(from, to)?;
        let target = ctx.candidate(&to)?;

        let path = reachable.path_nodes().collect_vec();
        let context = TransitionContext::new(ctx, reachable.candidates(), &path)?
            .with_resolution_method(reachable.resolution_method);

        let cost = target
            .emission
            .saturating_add(transition.heuristics.transition(context))
            .saturating_add(source_emission)
            .min(MAX_WEIGHT);

        Some((cost, reachable))
    }

    /// One source's outgoing weights (one row of a boundary's matrix, `NO_EDGE`
    /// where absent) plus the hops it produced.
    fn weigh_source<Emmis, Trans>(
        &self,
        ctx: &RoutingContext<E, M, N>,
        transition: &Transition<Emmis, Trans, E, M, N>,
        source: CandidateId,
        to_layer: &[CandidateId],
        first_layer: bool,
    ) -> (Vec<u32>, Hops<E>)
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
    {
        let mut row = vec![NO_EDGE; to_layer.len()];
        let mut hops = Hops::new();

        let Some(candidate) = ctx.candidate(&source) else {
            return (row, hops);
        };
        let source_emission = if first_layer { candidate.emission } else { 0 };

        for to in self.select(ctx, &candidate, to_layer) {
            let target = to_layer[to.index()];
            if let Some((cost, reachable)) =
                self.hop(ctx, transition, source, target, source_emission)
            {
                row[to.index()] = cost;
                hops.push(((source, target), reachable));
            }
        }

        (row, hops)
    }

    /// One boundary's dense row-major weight matrix (source rows stacked in order)
    /// plus all its hops.
    fn weigh_boundary<Emmis, Trans>(
        &self,
        ctx: &RoutingContext<E, M, N>,
        transition: &Transition<Emmis, Trans, E, M, N>,
        boundary: LayerId,
    ) -> (Vec<u32>, Hops<E>)
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
    {
        let (from_layer, to_layer) = transition.boundary(boundary);
        let first_layer = boundary.index() == 0;

        let mut matrix = Vec::with_capacity(from_layer.len() * to_layer.len());
        let mut hops = Hops::new();

        for &source in from_layer {
            let (row, source_hops) =
                self.weigh_source(ctx, transition, source, to_layer, first_layer);
            matrix.extend(row);
            hops.extend(source_hops);
        }

        (matrix, hops)
    }

    /// Weigh every **pending** boundary of `trellis` (resolved boundaries are left
    /// untouched), recording each hop into `side`. Boundaries weigh in parallel.
    fn weigh<Emmis, Trans>(
        &self,
        transition: &Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
        trellis: &mut Trellis,
        side: &mut SideTable<E>,
    ) -> Result<(), MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
        Self: Sync,
    {
        let ctx = transition.context(runtime);

        let pending = trellis
            .boundaries()
            .filter(|&boundary| !trellis.is_resolved(boundary))
            .collect::<Vec<_>>();

        let weighed = pending
            .par_iter()
            .map(|&boundary| (boundary, self.weigh_boundary(&ctx, transition, boundary)))
            .collect::<Vec<_>>();

        for (boundary, (matrix, hops)) in weighed {
            // A boundary nothing could bridge is left Pending rather than
            // resolved-but-empty: an unresolved boundary is exactly how the
            // trellis records a gap (see `Trellis::disconnections`).
            if matrix.iter().all(|&w| w == NO_EDGE) {
                continue;
            }

            trellis.fill_transition(boundary, &matrix)?;
            side.extend(hops);
        }

        Ok(())
    }

    /// Solve `transition` into the caller-owned `trellis`: weigh every pending
    /// boundary, find the minimum-cost path, and reconstruct the routed match.
    ///
    /// `trellis` is the caller's so it can be pre-sized, inspected, or reused; see
    /// [`Transition::solve`] for the entry that allocates one.
    fn solve<Emmis, Trans>(
        &self,
        transition: Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
        trellis: &mut Trellis,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
        Self: Sync,
    {
        let mut side = SideTable::default();
        self.weigh(&transition, runtime, trellis, &mut side)?;

        let layers = &transition.layers.layers;
        let disconnected = |breaks: Vec<LayerId>| -> MatchError {
            let breaks = breaks
                .into_iter()
                .map(|boundary| {
                    let (from, to) = (boundary.index(), boundary.index() + 1);
                    Disconnected {
                        from_layer: from,
                        to_layer: to,
                        from_origin: layers[from].origin,
                        to_origin: layers[to].origin,
                    }
                })
                .collect::<Vec<_>>();
            DisconnectedError { breaks }.into()
        };

        // Gaps: boundaries the weigher left Pending because nothing bridged them.
        let gaps = trellis.disconnections();
        if !gaps.is_empty() {
            return Err(disconnected(gaps));
        }

        let path = ViterbiSolver::new().solve(trellis)?;
        if !path.reachable {
            // Every boundary resolved, yet the reachable frontier dies mid-way:
            // some boundary has edges but none continue a live path. `Pending`
            // can't express this, so walk the resolved weights to find where.
            return Err(disconnected(frontier_collapse(trellis)));
        }

        let route = transition.route_of(&path);
        Ok(CollapsedPath::assemble(
            path.cost,
            route,
            &side,
            transition.candidates,
        ))
    }
}

/// Boundaries where a fully-resolved trellis still cannot carry a route: the
/// reachable frontier (all of layer 0, then propagated forward) dies because a
/// boundary's edges lead nowhere live. Reachability restarts past each break so
/// independent collapses downstream also surface.
///
/// This is the frontier-collapse residual that `Trellis::disconnections`
/// (Pending gaps) cannot see; every boundary here is resolved and has edges.
fn frontier_collapse(trellis: &Trellis) -> Vec<LayerId> {
    let widths = trellis.widths();
    let mut reachable = (0..widths[0] as usize).collect::<Vec<_>>();
    let mut breaks = Vec::new();

    for boundary in trellis.boundaries() {
        let to_width = widths[boundary.index() + 1] as usize;

        // A target is reachable when a reachable source has a present edge to it;
        // absent edges sit above the weight ceiling.
        let next = trellis
            .layer(boundary)
            .map(|matrix| {
                (0..to_width)
                    .filter(|&t| {
                        reachable
                            .iter()
                            .any(|&s| matrix[s * to_width + t] <= MAX_WEIGHT)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if next.is_empty() {
            breaks.push(boundary);
            reachable = (0..to_width).collect();
        } else {
            reachable = next;
        }
    }

    breaks
}
