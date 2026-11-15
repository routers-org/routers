//! The batched form of [`Weigher::hop`]: weighing a whole boundary at once.
//!
//! Little of a hop's geometry is unique to its pair. A candidate's heading,
//! tail and joint belong to the candidate ([`Anchor`]); the length, bearing
//! and turn of each road segment belong to the search tree it lies on
//! ([`Tree`]), which every hop from the same root shares — in this boundary,
//! the next, and every later match against the same cache.
//!
//! The arithmetic is unchanged: the same values, combined in the same order,
//! so the weights are bit-for-bit those of [`Weigher::hop`].

use std::sync::{PoisonError, RwLockReadGuard};

use geo::Point;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use routers_network::{Entry, Network, Node};
use routers_trellis::{MAX_WEIGHT, NO_EDGE};
use rustc_hash::FxHashMap;

use crate::{
    candidate::Candidate,
    costing::{
        Costing, CostingStrategies, EmissionStrategy, Headings, HopSummary, Strategy,
        TransitionContext, TransitionLengths, TransitionStrategy, VirtualTails,
    },
    map_path::{Joint, MapPath, Segment, Station, Turn, Turns},
    primitives::{
        Geometry, ResolutionMethod, RoutingContext,
        tree::{Leaf, Span, Tree},
    },
    weigh::Weigher,
};

/// The geometry of a candidate that every hop through it shares.
#[derive(Clone, Copy)]
struct Anchor {
    /// The candidate's position, ready to be measured against the positions
    /// of the candidates across the boundary.
    station: Station,
    /// Bearing of the candidate's edge; see [`Candidate::edge_heading`].
    heading: Option<f64>,
    /// The joint between the candidate's position and the end of its edge
    /// that a hop passes through. Its length is the virtual tail.
    joint: Option<Joint>,
}

impl Anchor {
    /// A candidate being left: it joins the route at its edge's target,
    /// turning off its edge onto the joint.
    fn departing<N: Network>(map: &N, candidate: &Candidate<N::Entry>) -> Self {
        let (source, target) = endpoints(map, candidate);
        let heading = edge_heading(source, target);

        Self {
            station: Station::at(candidate.position),
            heading,
            joint: target.map(|target| {
                let segment = Segment::between(candidate.position, target);

                Joint {
                    position: candidate.position,
                    segment,
                    turn: heading
                        .zip(segment.heading())
                        .map(|(edge, joint)| Turn::between(edge, joint)),
                }
            }),
        }
    }

    /// A candidate being arrived at: the route joins it at its edge's source,
    /// turning off the joint onto its edge.
    fn arriving<N: Network>(map: &N, candidate: &Candidate<N::Entry>) -> Self {
        let (source, target) = endpoints(map, candidate);
        let heading = edge_heading(source, target);

        Self {
            station: Station::at(candidate.position),
            heading,
            joint: source.map(|source| {
                let segment = Segment::between(source, candidate.position);

                Joint {
                    position: candidate.position,
                    segment,
                    turn: segment
                        .heading()
                        .zip(heading)
                        .map(|(joint, edge)| Turn::between(joint, edge)),
                }
            }),
        }
    }

    fn tail(&self) -> Option<f64> {
        self.joint.map(|joint| joint.segment.distance)
    }
}

fn endpoints<N: Network>(
    map: &N,
    candidate: &Candidate<N::Entry>,
) -> (Option<Point>, Option<Point>) {
    (
        map.point(&candidate.edge.source),
        map.point(&candidate.edge.target),
    )
}

fn edge_heading(source: Option<Point>, target: Option<Point>) -> Option<f64> {
    Segment::between(source?, target?).heading()
}

/// The buffers a [`Departure`] works in. They outlive it, so that a run of
/// departures allocates for its widest, not once each.
struct Scratch<E: Entry> {
    /// Where each candidate of the next layer sits in the departure's tree.
    leaves: Vec<Leaf>,
    /// The part of the tree the hops to `leaves` cross.
    span: Span,
    /// The turns taken from the current source to each step of `span`. Empty
    /// when they could not be carried there, for want of a span or of the
    /// source's joint onto the tree.
    turns: Vec<Turns>,

    /// One hop's path, where it has to be gathered.
    links: Vec<u32>,
    nodes: Vec<E>,
}

impl<E: Entry> Default for Scratch<E> {
    fn default() -> Self {
        Self {
            leaves: Vec::new(),
            span: Span::default(),
            turns: Vec::new(),
            links: Vec::new(),
            nodes: Vec::new(),
        }
    }
}

/// Weighs the hops leaving one road node: every source candidate of a
/// boundary whose edge ends there.
struct Departure<'a, 's, N, W, Emmis, Trans>
where
    N: Network,
    W: ?Sized,
    Emmis: EmissionStrategy,
    Trans: TransitionStrategy<N::Entry>,
{
    weigher: &'a W,
    ctx: &'a RoutingContext<'a, N>,
    costing: &'a CostingStrategies<Emmis, Trans, N::Entry>,

    /// The root's walked geometry. `None` when the map cannot place the
    /// root; hops then fall back.
    tree: Option<RwLockReadGuard<'a, Tree<N::Entry>>>,
    /// Whether the strategy leaves the paths unwalked, so that their turns
    /// are carried down the tree's span instead.
    spanned: bool,

    scratch: &'s mut Scratch<N::Entry>,
}

impl<'a, 's, N, W, Emmis, Trans> Departure<'a, 's, N, W, Emmis, Trans>
where
    N: Network,
    W: Weigher<N> + ?Sized,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<N::Entry> + Send + Sync,
{
    fn new(
        weigher: &'a W,
        ctx: &'a RoutingContext<'a, N>,
        costing: &'a CostingStrategies<Emmis, Trans, N::Entry>,
        geometry: Option<&'a Geometry<N::Entry>>,
        parents: &FxHashMap<N::Entry, N::Entry>,
        to_layer: &[Candidate<N::Entry>],
        scratch: &'s mut Scratch<N::Entry>,
    ) -> Self {
        scratch.leaves.clear();
        let tree = geometry
            .map(|geometry| locate(geometry, parents, ctx.map, to_layer, &mut scratch.leaves));

        let spanned = !<Trans as Strategy<TransitionContext<'a, N::Entry>>>::WALKS_PATH;
        if let (true, Some(tree)) = (spanned, tree.as_deref()) {
            let leaves = scratch.leaves.iter().filter_map(|leaf| match leaf {
                Leaf::At(leaf) => Some(*leaf),
                _ => None,
            });

            tree.span(leaves, &mut scratch.span);
        }

        Self {
            weigher,
            ctx,
            costing,
            tree,
            spanned,
            scratch,
        }
    }

    /// One row of the boundary's matrix: `source`'s weight to each of
    /// `to_layer`, `NO_EDGE` where absent.
    fn row(
        &mut self,
        source: &Candidate<N::Entry>,
        to_layer: &[Candidate<N::Entry>],
        arriving: &[Anchor],
        row: &mut [u32],
    ) {
        let departing = Anchor::departing(self.ctx.map, source);
        self.depart(&departing);

        for to in self.weigher.select(self.ctx, source, to_layer) {
            let (target, arriving) = (&to_layer[to.index()], &arriving[to.index()]);

            if let Some(cost) = self.hop(source, &departing, target, arriving, to.index()) {
                row[to.index()] = cost;
            }
        }
    }

    /// Carry the turns of the source leaving by `departing` down the span,
    /// so that each hop of its row finds the turns of its path, in path
    /// order, waiting at its leaf.
    fn depart(&mut self, departing: &Anchor) {
        let Scratch { span, turns, .. } = &mut *self.scratch;
        turns.clear();

        let (true, Some(tree), Some(joint)) = (self.spanned, self.tree.as_deref(), departing.joint)
        else {
            return;
        };

        turns.reserve(span.descent().len() + 1);
        turns.push(Turns::leaving(departing.heading, joint.segment, joint.turn));
        for step in span.descent() {
            let onward = turns[step.parent as usize].along(tree.link(step.link).stride);
            turns.push(onward);
        }
    }

    /// The hop to `leaf` as its summary, when the turns of the row's source
    /// were [carried](Self::depart) there and `arriving` can close them.
    ///
    /// A hop ending on the root itself crosses no edge and is costed as a
    /// path of no nodes, which has no summary of this kind.
    fn summary(
        &self,
        tree: &Tree<N::Entry>,
        leaf: u32,
        (source, departing): (&Candidate<N::Entry>, &Anchor),
        (target, arriving): (&Candidate<N::Entry>, &Anchor),
    ) -> Option<HopSummary> {
        if tree.is_root(leaf) {
            return None;
        }

        // No turns were carried anywhere unless the tree was spanned.
        let turns = self.scratch.turns.get(self.scratch.span.step(leaf)?)?;
        let joint = arriving.joint?;

        // As `TransitionContext::lengths`: the road between the two edges,
        // and each candidate's tail along its own.
        let lengths = departing
            .tail()
            .zip(arriving.tail())
            .map(|(leaving, entering)| TransitionLengths {
                straightline_distance: departing.station.distance(&arriving.station),
                route_length: tree.link(leaf).length + (leaving + entering),
            });

        Some(HopSummary {
            lengths,
            angular_complexity: turns.arriving(joint.segment, joint.turn, arriving.heading),
            classes: (source.edge.weight, target.edge.weight),
        })
    }

    /// [`Weigher::hop`], from shared geometry.
    fn hop(
        &mut self,
        source: &Candidate<N::Entry>,
        departing: &Anchor,
        target: &Candidate<N::Entry>,
        arriving: &Anchor,
        to: usize,
    ) -> Option<u32> {
        self.scratch.nodes.clear();

        let (path, method) = if source.directly_reachable(target, self.ctx.map)? {
            (MapPath::new([]), ResolutionMethod::DistanceOnly)
        } else {
            let Some(tree) = self.tree.as_deref() else {
                return self.fallback(source, target);
            };

            let leaf = match self.scratch.leaves[to] {
                Leaf::At(leaf) => leaf,
                Leaf::Unreachable => return None,
                Leaf::Unplaced => return self.fallback(source, target),
            };

            // A strategy that does not walk the path is costed from the
            // hop's summary: no nodes gathered, no context built around
            // them.
            if let Some(hop) = self.summary(tree, leaf, (source, departing), (target, arriving)) {
                let cost = self.costing.transition.summarised_cost(&hop);
                return Some(cost.min(MAX_WEIGHT));
            }

            let path = {
                let Scratch { links, nodes, .. } = &mut *self.scratch;
                tree.path(leaf, links);

                // A route that starts and ends on the same node crosses
                // no edge, and so has no path at all — not a path of one
                // node.
                if links.len() < 2 {
                    links.clear();
                }

                let links = links.iter().map(|&link| tree.link(link));

                nodes.extend(links.clone().map(|link| link.node.id));

                MapPath::with_geometry(
                    links.clone().map(|link| link.node).collect(),
                    links.clone().skip(1).map(|link| link.stride).collect(),
                    links.clone().next_back().map_or(0.0, |leaf| leaf.length),
                    departing.joint,
                    arriving.joint,
                )
            };

            (path, ResolutionMethod::Standard)
        };

        let context = TransitionContext::from_parts(
            self.ctx.candidates,
            (source, target),
            path,
            &self.scratch.nodes,
            Headings {
                source: departing.heading,
                target: arriving.heading,
            },
            VirtualTails {
                source: departing.tail(),
                target: arriving.tail(),
            },
            method,
        );

        Some(self.costing.transition(context).min(MAX_WEIGHT))
    }

    /// The pair-at-a-time hop, for the maps too irregular to share geometry
    /// over (a routed node with no position).
    #[cold]
    fn fallback(&self, source: &Candidate<N::Entry>, target: &Candidate<N::Entry>) -> Option<u32> {
        self.weigher
            .hop(self.ctx, self.costing, source.location, target.location)
    }
}

/// Find where each candidate of `to_layer` sits in the tree, measuring
/// whatever part of the way there is new, and hand the tree back held for
/// reading. A root weighed before is only read, so departures sharing it do
/// not wait on one another.
fn locate<'a, N>(
    geometry: &'a Geometry<N::Entry>,
    parents: &FxHashMap<N::Entry, N::Entry>,
    map: &N,
    to_layer: &[Candidate<N::Entry>],
    leaves: &mut Vec<Leaf>,
) -> RwLockReadGuard<'a, Tree<N::Entry>>
where
    N: Network,
{
    let targets = to_layer.iter().map(|target| target.edge.source);

    {
        let tree = geometry.read().unwrap_or_else(PoisonError::into_inner);

        leaves.extend(targets.clone().map_while(|leaf| tree.find(&leaf)));
        if leaves.len() == to_layer.len() {
            return tree;
        }
    }

    {
        let mut tree = geometry.write().unwrap_or_else(PoisonError::into_inner);

        leaves.clear();
        leaves.extend(targets.map(|leaf| tree.resolve(leaf, parents, map)));
    }

    geometry.read().unwrap_or_else(PoisonError::into_inner)
}

/// One boundary's dense row-major weight matrix, between `from_layer` and
/// `to_layer`.
///
/// Sources are grouped by the road node their edge ends at, since those share
/// a Dijkstra tree; groups weigh in parallel.
pub(crate) fn weigh<N, W, Emmis, Trans>(
    weigher: &W,
    ctx: &RoutingContext<N>,
    costing: &CostingStrategies<Emmis, Trans, N::Entry>,
    from_layer: &[Candidate<N::Entry>],
    to_layer: &[Candidate<N::Entry>],
) -> Vec<u32>
where
    N: Network,
    W: Weigher<N> + Sync + ?Sized,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<N::Entry> + Send + Sync,
{
    let width = to_layer.len();
    let mut matrix = vec![NO_EDGE; from_layer.len() * width];
    if matrix.is_empty() {
        return matrix;
    }

    let arriving = to_layer
        .iter()
        .map(|target| Anchor::arriving(ctx.map, target))
        .collect::<Vec<_>>();

    // Order rows by root so that each run of equal roots is one departure,
    // then hand each run the matrix rows it owns.
    let mut order = (0..from_layer.len() as u32).collect::<Vec<_>>();
    order.sort_unstable_by_key(|&row| (from_layer[row as usize].edge.target, row));

    let departures = order
        .chunk_by(|&a, &b| from_layer[a as usize].edge.target == from_layer[b as usize].edge.target)
        .collect::<Vec<_>>();

    // One reach for the whole boundary: how far apart its two layers are
    // decides how far any hop between them may route.
    let cache = weigher.cache().reaching(from_layer, to_layer);

    let weigh = |scratch: &mut Scratch<N::Entry>, sources: &&[u32]| {
        let root = from_layer[sources[0] as usize].edge.target;

        let parents = cache.query(ctx, root);
        let geometry = ctx
            .map
            .point(&root)
            .map(|position| cache.geometry(Node::new(position, root)));

        let mut departure = Departure::new(
            weigher,
            ctx,
            costing,
            geometry.as_ref(),
            &parents,
            to_layer,
            scratch,
        );
        let mut rows = vec![NO_EDGE; sources.len() * width];

        for (&source, row) in sources.iter().zip(rows.chunks_exact_mut(width)) {
            departure.row(&from_layer[source as usize], to_layer, &arriving, row);
        }

        rows
    };

    cfg_if::cfg_if! {
        if #[cfg(feature = "parallel")] {
            let weighed = departures
                .par_iter()
                .with_min_len(4)
                .map_init(Scratch::default, weigh)
                .collect::<Vec<_>>();
        } else {
            let mut scratch = Scratch::default();
            let weighed = departures
                .iter()
                .map(|sources| weigh(&mut scratch, sources))
                .collect::<Vec<_>>();
        }
    }

    for (sources, rows) in departures.iter().zip(weighed) {
        for (&source, row) in sources.iter().zip(rows.chunks_exact(width)) {
            let start = source as usize * width;
            matrix[start..start + width].copy_from_slice(row);
        }
    }

    matrix
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use geo::{Point, point};
    use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
    use routers_trellis::LayerId;

    use routers_network::Entry;

    use crate::candidate::Candidate;
    use crate::costing::EmissionStrategy;
    use crate::costing::{
        CostingStrategies, DefaultEmissionCost, DefaultTransitionCost, Strategy, TransitionContext,
        TransitionStrategy,
    };
    use crate::layer::generation::StandardGenerator;
    use crate::matcher::TripState;
    use crate::primitives::{PredicateCache, RoutingContext};
    use crate::weigh::{AllCompute, Selective, Weigher};
    use crate::{Matcher, Origin};
    use routers_trellis::NodeId;
    use uom::si::f64::Length;
    use uom::si::length::meter;

    const SIDE: i64 = 6;

    /// A city-block grid, every street two-way, ~90 m between junctions —
    /// so that hops turn corners, double back and share roots.
    fn grid() -> MockNetwork {
        let id = |row: i64, column: i64| row * SIDE + column + 1;

        let mut builder = MockNetworkBuilder::new();
        for row in 0..SIDE {
            for column in 0..SIDE {
                let position = point!(
                    x: -118.15 + column as f64 * 0.001,
                    y: 34.15 + row as f64 * 0.0008
                );
                builder = builder.node(id(row, column), position);
            }
        }
        for row in 0..SIDE {
            for column in 0..SIDE {
                if column + 1 < SIDE {
                    builder = builder
                        .edge(id(row, column), id(row, column + 1))
                        .edge(id(row, column + 1), id(row, column));
                }
                if row + 1 < SIDE {
                    builder = builder
                        .edge(id(row, column), id(row + 1, column))
                        .edge(id(row + 1, column), id(row, column));
                }
            }
        }
        builder.build()
    }

    /// East along one street, north up another, then back west: straight
    /// runs, right-angle turns, a U-turn's worth of heading change, and
    /// consecutive points on one edge.
    fn wander() -> Vec<Origin> {
        [
            (-118.1497, 34.15005),
            (-118.1492, 34.15004),
            (-118.1487, 34.15006),
            (-118.1479, 34.15003),
            (-118.14702, 34.1503),
            (-118.14698, 34.1509),
            (-118.14703, 34.1515),
            (-118.1474, 34.15161),
            (-118.1481, 34.15158),
            (-118.1489, 34.15162),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (x, y))| Origin::new(Point::new(x, y), index as i64))
        .collect()
    }

    /// The default transition cost, as a strategy that has not said whether
    /// it walks the path — and so is handed every path whole.
    struct Walking;

    impl<'a, E: Entry> Strategy<TransitionContext<'a, E>> for Walking {
        type Cost = f64;

        const ZETA: f64 = 1.;
        const BETA: f64 = 1.;

        fn calculate(&self, context: TransitionContext<'a, E>) -> Option<f64> {
            // What a strategy that walks the path is owed: the path.
            assert_eq!(
                context.optimal_path.linestring().0.len(),
                context.map_path.len()
            );

            DefaultTransitionCost.calculate(context)
        }
    }

    /// Weighing a boundary as a batch must cost every hop exactly as hopping
    /// pair by pair does — on a cold cache and again once its geometry is
    /// warm.
    fn matches_hop_by_hop<W: Weigher<MockNetwork> + Sync>(weigher: W) -> usize {
        costs_hop_by_hop(&weigher, DefaultTransitionCost) + costs_hop_by_hop(&weigher, Walking)
    }

    /// As [`matches_hop_by_hop`], under one transition strategy: the paths
    /// are summarised for one that does not walk them, and gathered for one
    /// that does.
    fn costs_hop_by_hop<W, Trans>(weigher: &W, transition: Trans) -> usize
    where
        W: Weigher<MockNetwork> + Sync,
        Trans: TransitionStrategy<MockEntryId> + Send + Sync,
    {
        let map = grid();
        let costing = CostingStrategies::new(DefaultEmissionCost::default(), transition);
        let generator = StandardGenerator::new(&map, &costing.emission);
        let matcher = Matcher::new(&map, &costing, generator, AllCompute::default(), &());

        let mut trip = matcher.begin();
        matcher.extend(&mut trip, &wander()).expect("trip anchors");

        let ctx = RoutingContext {
            candidates: trip.candidates(),
            map: &map,
            runtime: &(),
        };

        let (mut hops, mut absent) = (0, 0);
        for pass in ["cold", "warm"] {
            for boundary in 0..trip.layers() as u32 - 1 {
                let from_layer = trip.layer(LayerId(boundary)).expect("layer");
                let to_layer = trip.layer(LayerId(boundary + 1)).expect("layer");

                let expected = from_layer
                    .iter()
                    .flat_map(|source| weigher.weigh_source(&ctx, &costing, source, to_layer))
                    .collect::<Vec<_>>();
                let batched = weigher.weigh_boundary(&ctx, &costing, LayerId(boundary));

                assert_eq!(batched, expected, "boundary {boundary}, {pass}");
                hops += expected.iter().filter(|&&w| w != super::NO_EDGE).count();
                absent += expected.iter().filter(|&&w| w == super::NO_EDGE).count();
            }
        }

        // The comparison is vacuous unless hops were actually routed.
        assert!(hops > 100, "only {hops} hops weighed");
        absent
    }

    #[test]
    fn all_compute_matches_hop_by_hop() {
        matches_hop_by_hop(AllCompute::default());
    }

    #[test]
    fn selective_matches_hop_by_hop() {
        matches_hop_by_hop(Selective::default().with_fanout(3));
    }

    #[test]
    fn a_shared_cache_matches_hop_by_hop() {
        let cache = Arc::new(PredicateCache::default());

        matches_hop_by_hop(AllCompute::default().use_cache(cache.clone()));
        matches_hop_by_hop(Selective::default().use_cache(cache));
    }

    /// With too short a reach to cross the trip's corners, some targets lie
    /// beyond it — and are absent from both, not costed by one.
    #[test]
    fn a_short_reach_matches_hop_by_hop() {
        let short = || {
            Arc::new(PredicateCache::with_reach_distance(Length::new::<meter>(
                120.0,
            )))
        };

        let absent = matches_hop_by_hop(AllCompute::default().use_cache(short()));
        assert!(absent > 0, "no target was out of reach");
    }

    /// A weigher that weighs its own way, by overriding `weigh_boundary`.
    struct Flat(AllCompute<MockNetwork>);

    impl Weigher<MockNetwork> for Flat {
        fn cache(&self) -> &PredicateCache<MockNetwork> {
            self.0.cache()
        }

        fn select(
            &self,
            ctx: &RoutingContext<MockNetwork>,
            source: &Candidate<MockEntryId>,
            to_layer: &[Candidate<MockEntryId>],
        ) -> Vec<NodeId> {
            self.0.select(ctx, source, to_layer)
        }

        fn weigh_boundary<Emmis, Trans>(
            &self,
            ctx: &RoutingContext<MockNetwork>,
            _costing: &CostingStrategies<Emmis, Trans, MockEntryId>,
            boundary: LayerId,
        ) -> Vec<u32>
        where
            Emmis: EmissionStrategy + Send + Sync,
            Trans: TransitionStrategy<MockEntryId> + Send + Sync,
        {
            let width = |layer: u32| ctx.candidates.layer(LayerId(layer)).map_or(0, <[_]>::len);

            vec![7; width(boundary.0) * width(boundary.0 + 1)]
        }
    }

    /// `weigh` must weigh through `weigh_boundary`: a weigher that overrides
    /// it is otherwise ignored, without a word.
    #[test]
    fn weighing_honours_an_overridden_boundary() {
        let map = grid();
        let costing = CostingStrategies::default();
        let generator = StandardGenerator::new(&map, &costing.emission);
        let matcher = Matcher::new(&map, &costing, generator, AllCompute::default(), &());

        let mut trip = matcher.begin();
        matcher.extend(&mut trip, &wander()).expect("trip anchors");

        let TripState::Building(mut trellis) = trip.state.clone() else {
            panic!("an extended trip is building");
        };
        let ctx = RoutingContext {
            candidates: trip.candidates(),
            map: &map,
            runtime: &(),
        };

        Flat(AllCompute::default())
            .weigh(&ctx, &costing, &mut trellis)
            .expect("weighs");

        for boundary in trellis.boundaries() {
            let weights = trellis.layer(boundary).expect("boundary is resolved");
            assert!(weights.iter().all(|&weight| weight == 7), "{boundary:?}");
        }
    }

    /// A strategy that opts out of the path, and never says how to cost a
    /// hop without it.
    struct Silent;

    impl<'a, E: Entry> Strategy<TransitionContext<'a, E>> for Silent {
        type Cost = f64;

        const ZETA: f64 = 1.;
        const BETA: f64 = 1.;
        const WALKS_PATH: bool = false;

        fn calculate(&self, context: TransitionContext<'a, E>) -> Option<f64> {
            DefaultTransitionCost.calculate(context)
        }
    }

    /// ...which must fail loudly, not price every routed hop at the ceiling.
    #[test]
    #[should_panic(expected = "must implement `Strategy::summarised`")]
    fn opting_out_of_the_path_needs_a_summary() {
        costs_hop_by_hop(&AllCompute::default(), Silent);
    }
}
