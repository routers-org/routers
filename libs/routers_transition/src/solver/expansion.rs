//! Candidate expansion: how one candidate is reached from another on the road
//! network. Shared by every solver strategy.

use core::hash::Hash;

use routers_network::{Edge, Entry, Metadata, Network};
use rustc_hash::FxHashMap;

use crate::{Candidate, CandidateId, PredicateCache, Reachable, RoutingContext};

/// A parent-pointer map — each node mapped to its parent (and the cost to it) —
/// as produced by the predicate cache's bounded Dijkstra.
trait ParentPath<K> {
    /// The nodes from `root` to `leaf` inclusive, followed via parent pointers,
    /// or `None` if `leaf` is absent from the map.
    fn path(&self, root: &K, leaf: &K) -> Option<Vec<K>>;
}

impl<K, C> ParentPath<K> for FxHashMap<K, (K, C)>
where
    K: Eq + Hash + Copy,
{
    fn path(&self, root: &K, leaf: &K) -> Option<Vec<K>> {
        let mut nodes = vec![*leaf];
        let mut cursor = leaf;

        while cursor != root {
            let (parent, _) = self.get(cursor)?;
            nodes.push(*parent);
            cursor = parent;
        }

        nodes.reverse();
        Some(nodes)
    }
}

/// Answers "how is candidate `to` reached from candidate `from`?" against the
/// road network, pairing the [`RoutingContext`] with the [`PredicateCache`] that
/// serves bounded reachability queries.
pub(crate) struct Expansion<'a, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    ctx: &'a RoutingContext<'a, E, M, N>,
    predicate: &'a PredicateCache<E, M, N>,
}

impl<'a, E, M, N> Expansion<'a, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    pub(crate) fn new(
        ctx: &'a RoutingContext<'a, E, M, N>,
        predicate: &'a PredicateCache<E, M, N>,
    ) -> Self {
        Self { ctx, predicate }
    }

    /// The [`Reachable`] describing how `to` is reached from `from`, or `None`
    /// when `to` lies outside the predicate bound.
    ///
    /// Candidates already sharing a directed edge resolve directly (by distance);
    /// otherwise the routed path between their edges is walked from the predicate
    /// map.
    pub(crate) fn reach(&self, from: CandidateId, to: CandidateId) -> Option<Reachable<E>> {
        let source = self.ctx.candidate(&from)?;
        let target = self.ctx.candidate(&to)?;

        if source.directly_reachable(&target, self.ctx.map)? {
            return Some(Reachable::direct(from, to));
        }

        Some(Reachable::new(from, to, self.route(&source, &target)?))
    }

    /// The road edges linking `source`'s edge to `target`'s edge, walked from the
    /// bounded-Dijkstra predicate map rooted at `source`'s edge target.
    fn route(&self, source: &Candidate<E>, target: &Candidate<E>) -> Option<Vec<Edge<E>>> {
        let parents = self.predicate.query(self.ctx, source.edge.target);
        let nodes = parents.path(&source.edge.target, &target.edge.source)?;

        Some(
            nodes
                .windows(2)
                .filter_map(|pair| match pair {
                    [a, b] => self.ctx.edge(a, b),
                    _ => None,
                })
                .collect(),
        )
    }
}
