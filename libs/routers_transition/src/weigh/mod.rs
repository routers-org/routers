//! Weighing: filling a trellis's pending boundaries with transition costs.
//!
//! A [`Weigher`] computes edge weights only. Emission costs enter the trellis
//! as node weights when a layer is pushed (see [`Matcher`](crate::Matcher)),
//! and the minimum-cost path is found by `routers_trellis`. Strategies differ
//! solely in [which next-layer candidates they weigh](Weigher::select).

mod all_compute;
mod boundary;
mod expansion;
mod selective;
mod variant;

pub use all_compute::AllCompute;
use cfg_if::cfg_if;
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
#[cfg(feature = "parallel")]
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
    /// The predicate cache backing this weigher's reachability queries. Its
    /// `reach_distance` is how far a query searches from a candidate before
    /// giving up.
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
    ///
    /// This is the hop-at-a-time reference. [`weigh_boundary`] produces the
    /// same rows without calling it (or [`hop`]), so a weigher that changes
    /// how a hop is costed must override [`weigh_boundary`] too.
    ///
    /// [`weigh_boundary`]: Self::weigh_boundary
    /// [`hop`]: Self::hop
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
    /// Equal to stacking [`weigh_source`](Self::weigh_source) over the
    /// boundary's sources, but weighed as a batch: geometry that many hops
    /// share — a candidate's heading, a road segment's length and bearing —
    /// is derived once for the boundary rather than once per hop. Sources
    /// weigh in parallel, so a boundary weighed alone — the realtime append —
    /// still uses the cores.
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

        boundary::weigh(self, ctx, costing, from_layer, to_layer)
    }

    /// Weigh every **pending** boundary of `trellis` (resolved boundaries are
    /// left untouched). Boundaries weigh in parallel.
    ///
    /// A boundary nothing could bridge is left `Pending` rather than
    /// resolved-but-empty: an unresolved boundary is exactly how the trellis
    /// records a gap (see [`Trellis::disconnections`]).
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            level = "debug",
            skip_all,
            fields(
                boundaries = tracing::field::Empty,
                hops = tracing::field::Empty,
                searches = tracing::field::Empty
            )
        )
    )]
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
        /// Hops a parallel task should hold, at the least. Narrow boundaries
        /// are a few hops each, so they are batched until a task is worth
        /// more than the cost of handing it to another thread.
        #[cfg(feature = "parallel")]
        const TASK_HOPS: usize = 32;

        let pending = trellis
            .boundaries()
            .filter(|&boundary| !trellis.is_resolved(boundary))
            .filter_map(|boundary| {
                let from_layer = ctx.candidates.layer(boundary)?;
                let to_layer = ctx.candidates.layer(LayerId(boundary.0 + 1))?;
                Some((boundary, from_layer, to_layer))
            })
            .collect::<Vec<_>>();

        #[cfg(feature = "tracing")]
        let searches = self.cache().searches();

        // Through `weigh_boundary`, so that a weigher which overrides it is
        // the one that weighs.
        let weigh = |&(boundary, _, _): &(LayerId, _, _)| {
            (boundary, self.weigh_boundary(ctx, costing, boundary))
        };

        cfg_if! {
            if #[cfg(feature = "parallel")] {
                let hops = pending
                    .iter()
                    .map(|(_, from_layer, to_layer)| from_layer.len() * to_layer.len())
                    .sum::<usize>();
                let batch = (TASK_HOPS * pending.len()).div_ceil(hops.max(1));

                let weighed = pending
                    .par_iter()
                    .with_min_len(batch)
                    .map(weigh)
                    .collect::<Vec<_>>();
            } else {
                let weighed = pending.iter().map(weigh).collect::<Vec<_>>();
            }
        }

        #[cfg(feature = "tracing")]
        {
            let span = tracing::Span::current();
            span.record("boundaries", pending.len());
            span.record(
                "hops",
                pending
                    .iter()
                    .map(|(_, from_layer, to_layer)| from_layer.len() * to_layer.len())
                    .sum::<usize>(),
            );
            span.record("searches", self.cache().searches() - searches);
        }

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
