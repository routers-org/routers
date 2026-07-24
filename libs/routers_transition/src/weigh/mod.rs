//! Weighing: filling a trellis's pending boundaries with transition costs.
//!
//! A [`Weigher`] computes edge weights only. Emission costs enter the trellis
//! as node weights when a layer is pushed (see [`Matcher`](crate::Matcher)),
//! and the minimum-cost path is found by `routers_trellis`. Strategies differ
//! solely in [which next-layer candidates they weigh](Weigher::select).

mod all_compute;
mod expansion;
mod selective;
mod variant;

pub use all_compute::AllCompute;
pub use selective::{DEFAULT_FANOUT, Selective};
pub use variant::SolverVariant;

use crate::{
    candidate::{Candidate, CandidateRef},
    costing::{
        Costing, CostingStrategies, EmissionStrategy, TransitionContext, TransitionStrategy,
    },
    primitives::{MatchError, PredicateCache, Reachable, RoutingContext},
    weigh::expansion::Expansion,
};
use itertools::Itertools;
use rayon::prelude::*;
use routers_network::Network;
use routers_trellis::{LayerId, MAX_WEIGHT, NO_EDGE, NodeId, Trellis};

/// A strategy for weighing the pending boundaries of a [`Trellis`].
///
/// Weighing touches only **pending** boundaries: resolved weights are
/// append-stable and never recomputed, so weighing a grown trellis costs only
/// the new boundaries.
pub trait Weigher<N>
where
    N: Network,
{
    /// The predicate cache backing this weigher's reachability queries.
    fn cache(&self) -> &PredicateCache<N>;

    /// Which next-layer candidates to weigh for `source`, as positions within
    /// `to_layer`. All-compute returns all of them; a selective strategy returns a
    /// promising subset.
    fn select(
        &self,
        ctx: &RoutingContext<N>,
        source: &Candidate<N::Entry>,
        to_layer: &[Candidate<N::Entry>],
    ) -> Vec<NodeId>;

    /// How candidate `to` is reached from `from` on the road network, or `None`
    /// when it is not. Also the collapse-time re-derivation of hop geometry —
    /// deterministic, so it reproduces exactly what weighing costed.
    fn reach(
        &self,
        ctx: &RoutingContext<N>,
        from: CandidateRef,
        to: CandidateRef,
    ) -> Option<Reachable<N::Entry>> {
        Expansion::new(ctx, self.cache()).reach(from, to)
    }

    /// The transition cost `from -> to`, or `None` when `to` is unreachable.
    /// Clamped to the trellis weight ceiling. Emission costs are *not* included
    /// here — they are node weights.
    fn hop<Emmis, Trans>(
        &self,
        ctx: &RoutingContext<N>,
        costing: &CostingStrategies<Emmis, Trans, N::Entry>,
        from: CandidateRef,
        to: CandidateRef,
    ) -> Option<u32>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<N::Entry> + Send + Sync,
    {
        let reachable = self.reach(ctx, from, to)?;

        let path = reachable.path_nodes().collect_vec();
        let context = TransitionContext::new(ctx, reachable.candidates(), &path)?
            .with_resolution_method(reachable.resolution_method);

        Some(costing.transition(context).min(MAX_WEIGHT))
    }

    /// One source's outgoing weights: one row of a boundary's matrix, `NO_EDGE`
    /// where absent.
    fn weigh_source<Emmis, Trans>(
        &self,
        ctx: &RoutingContext<N>,
        costing: &CostingStrategies<Emmis, Trans, N::Entry>,
        source: &Candidate<N::Entry>,
        to_layer: &[Candidate<N::Entry>],
    ) -> Vec<u32>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<N::Entry> + Send + Sync,
    {
        let mut row = vec![NO_EDGE; to_layer.len()];

        for to in self.select(ctx, source, to_layer) {
            let target = &to_layer[to.index()];
            if let Some(cost) = self.hop(ctx, costing, source.location, target.location) {
                row[to.index()] = cost;
            }
        }

        row
    }

    /// One boundary's dense row-major weight matrix (source rows stacked in order).
    ///
    /// Source rows weigh in parallel so that a boundary weighed alone — the
    /// realtime append — still uses the cores, but chunked (`with_min_len`) so
    /// narrow boundaries don't drown the row's work in task overhead.
    fn weigh_boundary<Emmis, Trans>(
        &self,
        ctx: &RoutingContext<N>,
        costing: &CostingStrategies<Emmis, Trans, N::Entry>,
        boundary: LayerId,
    ) -> Vec<u32>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<N::Entry> + Send + Sync,
        Self: Sync,
    {
        let (Some(from_layer), Some(to_layer)) = (
            ctx.candidates.layer(boundary),
            ctx.candidates.layer(LayerId(boundary.0 + 1)),
        ) else {
            return Vec::new();
        };

        from_layer
            .par_iter()
            .with_min_len(8)
            .map(|source| self.weigh_source(ctx, costing, source, to_layer))
            .flatten_iter()
            .collect()
    }

    /// Weigh every **pending** boundary of `trellis` (resolved boundaries are
    /// left untouched). Boundaries weigh in parallel.
    ///
    /// A boundary nothing could bridge is left `Pending` rather than
    /// resolved-but-empty: an unresolved boundary is exactly how the trellis
    /// records a gap (see [`Trellis::disconnections`]).
    fn weigh<Emmis, Trans>(
        &self,
        ctx: &RoutingContext<N>,
        costing: &CostingStrategies<Emmis, Trans, N::Entry>,
        trellis: &mut Trellis,
    ) -> Result<(), MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<N::Entry> + Send + Sync,
        Self: Sync,
    {
        let pending = trellis
            .boundaries()
            .filter(|&boundary| !trellis.is_resolved(boundary))
            .collect::<Vec<_>>();

        let weighed = pending
            .into_par_iter()
            .map(|boundary| (boundary, self.weigh_boundary(ctx, costing, boundary)))
            .collect::<Vec<_>>();

        for (boundary, matrix) in weighed {
            if matrix.iter().all(|&w| w == NO_EDGE) {
                continue;
            }

            trellis.fill_transition(boundary, &matrix)?;
        }

        Ok(())
    }
}

pub(crate) fn frontier_collapse(trellis: &Trellis) -> Vec<LayerId> {
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
