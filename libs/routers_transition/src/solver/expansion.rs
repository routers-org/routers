//! Shared candidate-expansion core.
//!
//! Computes, for a `(source, target)` candidate pair, whether the target is
//! reachable from the source on the underlying road network and by which path
//! ([`Reachable`]). This is the network-routing half of a solve and is shared by
//! every solver driver (eager trellis fill, lazy frontier `astar`).
//!
//! Extracted verbatim from the per-target logic in the forward solvers so the
//! drivers differ only in *how* they consume it (materialise all vs. on demand).

use core::hash::Hash;

use routers_network::{Entry, Metadata, Network};
use rustc_hash::FxHashMap;

use crate::{PredicateCache, Reachable, RoutingContext, candidate::CandidateId};

/// Reconstruct a path from `source` up the `parents` map to `target`, returned in
/// `[source, ..., target]` order. `None` if `target` is unreachable in the map.
///
/// (Free-function form of the former `Solver::path_builder`, so every driver and
/// the shared expansion core can call it without a solver instance.)
#[inline]
pub(crate) fn path_builder<K, C>(
    target: &K,
    source: &K,
    parents: &FxHashMap<K, (K, C)>,
) -> Option<Vec<K>>
where
    K: Eq + Hash + Copy,
{
    let mut path = vec![*target];
    let mut next = target;

    while next != source {
        let (parent, _) = parents.get(next)?;
        path.push(*parent);
        next = parent;
    }

    path.reverse();
    Some(path)
}

/// Derive the [`Reachable`] describing how `target_id` is reached from
/// `source_id`, or `None` if it is not reachable within the predicate bound.
///
/// Mirrors `PrecomputeForwardSolver::get_reachable` / the per-target body of
/// `SelectiveForwardSolver::reachable`:
/// - Same-edge candidates resolve to a [`ResolutionMethod::DistanceOnly`] hop when
///   tracking forward; otherwise they fall through to a routed path.
/// - Otherwise an upper-bounded Dijkstra predicate map is walked to build the
///   routed edge path between the two candidate edges.
pub(crate) fn reachable_between<E, M, N>(
    ctx: &RoutingContext<'_, E, M, N>,
    predicate: &PredicateCache<E, M, N>,
    source_id: &CandidateId,
    target_id: &CandidateId,
) -> Option<Reachable<E>>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    let source = ctx.candidate(source_id)?;
    let predicate_map = predicate.query(ctx, source.edge.target);
    let candidate = ctx.candidate(target_id)?;

    // Both candidates are on the same edge.
    'stmt: {
        if candidate.edge.id.index() == source.edge.id.index() {
            let common_source = candidate.edge.source == source.edge.source;
            let common_target = candidate.edge.target == source.edge.target;

            let tracking_forward = common_source && common_target;

            let source_percentage = source.percentage(ctx.map)?;
            let target_percentage = candidate.percentage(ctx.map)?;

            return if tracking_forward && source_percentage <= target_percentage {
                // Moving forward on the same edge — just the distance between them.
                Some(Reachable::new(*source_id, *target_id, vec![]).distance_only())
            } else {
                // Going "backwards" across the node is an independent transition,
                // not covered here.
                break 'stmt;
            };
        }
    }

    // Generate the path to this target using the predicate map.
    let path_to_target = path_builder(&candidate.edge.source, &source.edge.target, &predicate_map)?;

    let path = path_to_target
        .windows(2)
        .filter_map(|pair| {
            if let [a, b] = pair {
                return ctx.edge(a, b);
            }
            None
        })
        .collect::<Vec<_>>();

    Some(Reachable::new(*source_id, *target_id, path))
}
