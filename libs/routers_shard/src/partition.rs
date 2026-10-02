//! Single-pass shard production.

use geo::{Point, Rect};
use log::debug;
use rustc_hash::{FxHashMap, FxHashSet};
use web_time::Instant;

use routers_network::{DirectionAwareEdgeId, Entry, Metadata, Node, RowIndex, edge::Weight};

use crate::network::{GraphStructure, ShardSource, ShardedNetwork};
use crate::selection::{Selection, SelectionMode};
use crate::strategy::{ShardId, ShardingStrategy};

/// Closed-interval rectangle overlap, matching `padding_contains`.
fn intersects(a: &Rect, b: &Rect) -> bool {
    a.min().x <= b.max().x
        && b.min().x <= a.max().x
        && a.min().y <= b.max().y
        && b.min().y <= a.max().y
}

/// Everything a shard needs before its graph and indices are built.
struct Bucket<E: Entry> {
    /// Whether any node is owned by this cell, as opposed to admitted by
    /// its padding.
    owned: bool,
    nodes: Vec<E>,
    edges: Vec<(E, E, Weight, u32)>,
}

/// Edge metadata sentinel: see [`Bucket::edges`].
const NO_META: u32 = u32::MAX;

impl<E: Entry> Default for Bucket<E> {
    fn default() -> Self {
        Self {
            owned: false,
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

/// The padded selection of a cell and of each of its neighbours: the only
/// shards a point in that cell can belong to.
struct Neighbourhood<S: ShardId> {
    selections: Vec<Selection<S>>,
}

/// Resolves a point to the shards that admit it, memoising each cell's
/// neighbourhood so the strategy's neighbour walk runs once per cell.
struct Dealer<'a, St: ShardingStrategy> {
    strategy: &'a St,
    mode: SelectionMode,
    neighbourhoods: FxHashMap<St::Id, Neighbourhood<St::Id>>,
}

impl<St: ShardingStrategy> Dealer<'_, St> {
    fn cells_of(&mut self, pos: Point, out: &mut Vec<St::Id>) -> St::Id {
        let owner = self.strategy.locate(pos);
        let strategy = self.strategy;
        let mode = self.mode;
        let neighbourhood = self.neighbourhoods.entry(owner).or_insert_with(|| {
            let owner_bounds = strategy.bounds(&owner);

            let mut seen = FxHashSet::default();
            let mut queue = vec![owner];
            let mut selections = Vec::new();

            seen.insert(owner);

            while let Some(cell) = queue.pop() {
                let selection = Selection::new(strategy, cell, mode);
                let reaches = selection
                    .padding
                    .as_ref()
                    .is_some_and(|padded| intersects(padded, &owner_bounds));
                if !reaches {
                    continue;
                }

                for next in strategy.neighbours(&cell) {
                    if seen.insert(next) {
                        queue.push(next);
                    }
                }

                selections.push(selection);
            }

            Neighbourhood { selections }
        });

        out.clear();

        for selection in &neighbourhood.selections {
            if selection.owned == owner || selection.padding_contains(pos) {
                out.push(selection.owned);
            }
        }

        owner
    }
}

/// A source dealt into per-shard buckets, yielded shard by shard.
pub struct Partition<E: Entry, M: Metadata, S: ShardId> {
    build_indices: bool,
    positions: FxHashMap<E, Point>,
    metas: Vec<M>,
    buckets: alloc::vec::IntoIter<(S, Bucket<E>)>,
    remaining: usize,
}

impl<E, M, S> Partition<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    /// Deal `source` into buckets, one per shard that will hold data.
    pub fn new<Src, St>(source: &Src, strategy: &St, padding_distance: f64) -> Self
    where
        Src: ShardSource<E, M>,
        St: ShardingStrategy<Id = S>,
    {
        let started = Instant::now();
        let mode = SelectionMode::OwnedAndPadded { padding_distance };

        let mut dealer = Dealer {
            strategy,
            mode,
            neighbourhoods: FxHashMap::default(),
        };

        let mut positions: FxHashMap<E, Point> = FxHashMap::default();
        let mut buckets: FxHashMap<S, Bucket<E>> = FxHashMap::default();
        let mut metas: Vec<M> = Vec::new();

        // Shards a node was pulled into as the far end of an edge, over and
        // above those its position admits it to. Sparse: only boundary nodes.
        let mut rode_along: FxHashMap<E, Vec<S>> = FxHashMap::default();

        let mut seen_from: FxHashSet<E> = FxHashSet::default();
        let mut fresh: FxHashSet<E> = FxHashSet::default();
        let mut from_cells = Vec::with_capacity(9);
        let mut to_cells = Vec::with_capacity(9);

        for (id, pos) in source.nodes() {
            positions.insert(id, pos);
            let owner = dealer.cells_of(pos, &mut from_cells);
            for cell in &from_cells {
                let bucket = buckets.entry(*cell).or_default();
                bucket.nodes.push(id);
                bucket.owned |= *cell == owner;
            }
        }

        let dealt_nodes = started.elapsed();

        for (from, to, weight, meta) in source.edges() {
            // `from` must already be in the shard — by padding, or as the far
            // end of an earlier edge — and `to` must have a position, or the
            // edge is dropped, exactly as `from_source` sees it.
            let Some(&from_pos) = positions.get(&from) else {
                continue;
            };
            let Some(&to_pos) = positions.get(&to) else {
                continue;
            };

            dealer.cells_of(from_pos, &mut from_cells);
            if let Some(extra) = rode_along.get(&from) {
                from_cells.extend_from_slice(extra);
            }
            dealer.cells_of(to_pos, &mut to_cells);
            if let Some(extra) = rode_along.get(&to) {
                to_cells.extend_from_slice(extra);
            }

            let keeps_meta = seen_from.insert(from) | fresh.remove(&from);
            let meta_index = if keeps_meta {
                metas.push(meta);
                u32::try_from(metas.len() - 1).expect("fewer than 2^32 metadata")
            } else {
                NO_META
            };

            let mut rode = false;
            for cell in &from_cells {
                let bucket = buckets.get_mut(cell).expect("dealt");
                if !to_cells.contains(cell) {
                    // The far end rides along into any shard it is not
                    // already in.
                    rode_along.entry(to).or_default().push(*cell);
                    bucket.nodes.push(to);
                    rode = true;
                }
                bucket.edges.push((from, to, weight, meta_index));
            }
            if rode {
                fresh.insert(to);
            }
        }

        // Padding alone does not make a shard: `from_source` is only ever asked
        // for cells that own a node, so neither does this.
        let mut buckets: Vec<(S, Bucket<E>)> =
            buckets.into_iter().filter(|(_, b)| b.owned).collect();
        buckets.sort_by_key(|(id, _)| *id);

        debug!(
            "Partition::new dealt {} nodes into {} shards — nodes {:?}, edges {:?}",
            positions.len(),
            buckets.len(),
            dealt_nodes,
            started.elapsed() - dealt_nodes
        );

        Self {
            build_indices: true,
            positions,
            metas,
            remaining: buckets.len(),
            buckets: buckets.into_iter(),
        }
    }

    /// Yield shards without spatial indices. They are rebuilt on load
    /// anyway (`from_cached_bytes`), so a producer that only writes shards
    /// to disk saves two R-tree builds per shard.
    pub fn without_indices(mut self) -> Self {
        self.build_indices = false;
        self
    }

    /// Drop every shard whose id fails `keep` before it is built. Used to
    /// produce only the shards a chunk of a larger extract is responsible
    /// for, without paying for the ones its buffer zone would also yield.
    pub fn retain(&mut self, mut keep: impl FnMut(&S) -> bool) {
        let buckets = core::mem::replace(&mut self.buckets, Vec::new().into_iter());
        let kept: Vec<_> = buckets.filter(|(id, _)| keep(id)).collect();
        self.remaining = kept.len();
        self.buckets = kept.into_iter();
    }

    /// Shards still to be yielded.
    pub fn len(&self) -> usize {
        self.remaining
    }

    pub fn is_empty(&self) -> bool {
        self.remaining == 0
    }

    fn materialise(&self, owned: S, bucket: Bucket<E>) -> ShardedNetwork<E, M, S> {
        let mut graph: GraphStructure<E> = GraphStructure::new();
        let mut hash: FxHashMap<E, Node<E>> = FxHashMap::default();

        for id in bucket.nodes {
            let pos = self.positions[&id];
            hash.insert(id, Node::new(pos, id));
            graph.add_node(id);
        }

        let mut meta: FxHashMap<E, M> = FxHashMap::default();
        for (from, to, weight, meta_index) in bucket.edges {
            graph.add_edge(from, to, (weight, DirectionAwareEdgeId::new(from)));
            if meta_index != NO_META {
                meta.entry(from)
                    .or_insert_with(|| self.metas[meta_index as usize].clone());
            }
        }

        let mut loaded = FxHashSet::default();
        loaded.insert(owned);

        let mut net = ShardedNetwork {
            graph,
            hash,
            meta,
            index: RowIndex::default(),
            index_edge: RowIndex::default(),
            owned,
            loaded,
        };
        if self.build_indices {
            net.rebuild_indices();
        }
        net
    }
}

impl<E, M, S> Iterator for Partition<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    type Item = ShardedNetwork<E, M, S>;

    fn next(&mut self) -> Option<Self::Item> {
        let (owned, bucket) = self.buckets.next()?;
        self.remaining -= 1;
        Some(self.materialise(owned, bucket))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<E, M, S> ExactSizeIterator for Partition<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
}

impl<E, M, S> ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    /// Every shard of `source` in one pass. See [`Partition`].
    pub fn partition<Src, St>(
        source: &Src,
        strategy: &St,
        padding_distance: f64,
    ) -> Partition<E, M, S>
    where
        Src: ShardSource<E, M>,
        St: ShardingStrategy<Id = S>,
    {
        Partition::new(source, strategy, padding_distance)
    }
}
