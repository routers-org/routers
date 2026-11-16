use core::{
    f64::consts::PI,
    ops::{Div, Mul, Rem},
};

use geo::{Bearing, Distance, Haversine, LineString, Point};
use routers_network::{Entry, Network, Node};

/// The great-circle length and bearing between two positions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Segment {
    /// Haversine length, in metres.
    pub(crate) distance: f64,
    /// Haversine bearing in degrees.
    bearing: f64,
}

impl Segment {
    /// Segments shorter than this have no usable bearing.
    const MIN_HEADING_DISTANCE: f64 = 1.0;

    #[inline]
    pub(crate) fn between(from: Point, to: Point) -> Self {
        let distance = Haversine.distance(from, to);
        let bearing = if distance < Self::MIN_HEADING_DISTANCE {
            f64::NAN
        } else {
            Haversine.bearing(from, to)
        };

        Self { distance, bearing }
    }

    /// The bearing of the segment, undefined (`None`) for overlapping points.
    #[inline]
    pub(crate) fn heading(&self) -> Option<f64> {
        (self.distance >= Self::MIN_HEADING_DISTANCE).then_some(self.bearing)
    }
}

/// A position with the cosine of its latitude precomputed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Station {
    position: Point,
    /// `cos` of the latitude, in radians.
    cosine: f64,
}

impl Station {
    #[inline]
    pub(crate) fn at(position: Point) -> Self {
        Self {
            position,
            cosine: position.y().to_radians().cos(),
        }
    }

    /// `Haversine.distance(self, other)`, to the bit.
    #[inline]
    pub(crate) fn distance(&self, other: &Station) -> f64 {
        let (origin, destination) = (self.position, other.position);

        let delta_theta = (destination.y() - origin.y()).to_radians();
        let delta_lambda = (destination.x() - origin.x()).to_radians();
        let a = (delta_theta / 2.0).sin().powi(2)
            + self.cosine * other.cosine * (delta_lambda / 2.0).sin().powi(2);
        let c = 2.0 * a.sqrt().asin();

        Haversine.radius() * c
    }
}

/// One [`Segment`] of a path, with the cost of the turn taken onto it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stride {
    pub(crate) segment: Segment,

    /// The reciprocal [`turn_cost`] of the turn onto this segment; [`BLOCKED`]
    /// for a turn of zero cost, `None` when either segment has no heading.
    pub(crate) turn: Option<f64>,
}

/// The reciprocal standing for a turn of zero cost.
const BLOCKED: f64 = f64::INFINITY;

impl Stride {
    /// The stride following a segment whose last heading was `previous`.
    #[inline]
    pub(crate) fn after(previous: Option<f64>, segment: Segment) -> Self {
        let turn = previous.zip(segment.heading()).map(|(previous, heading)| {
            let cost = turn_cost(heading_delta(previous, heading));

            if cost <= 0.0 { BLOCKED } else { 1.0 / cost }
        });

        Self { segment, turn }
    }
}

/// The cost of the turn between two headings, measured ahead of time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Turn {
    from: f64,
    to: f64,
    cost: f64,
}

impl Turn {
    #[inline]
    pub(crate) fn between(from: f64, to: f64) -> Self {
        Self {
            from,
            to,
            cost: turn_cost(heading_delta(from, to)),
        }
    }
}

/// A [`Segment`] joining a candidate position to one end of the path.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Joint {
    pub(crate) position: Point,
    pub(crate) segment: Segment,
    /// The turn between this joint and the candidate's edge heading.
    pub(crate) turn: Option<Turn>,
}

/// For asking geometric questions of a path over the network, use a
/// [`MapPath`].
///
/// It wraps an ordered list of [`Node`]s and derives the geometry of the path
/// they trace: its [`length`](Self::length), its
/// [`headings`](Self::headings), and how much turning it exhibits
/// ([`total_angle`](Self::total_angle),
/// [`angular_complexity`](Self::angular_complexity)). The default transition
/// costing uses these to judge how plausible a candidate-to-candidate route
/// is.
#[derive(Clone, Debug)]
pub struct MapPath<E>
where
    E: Entry,
{
    nodes: Vec<Node<E>>,

    /// Precomputed geometry of each consecutive node pair and their summed
    /// length; `None` derives on demand.
    strides: Option<(Vec<Stride>, f64)>,

    /// Precomputed joints onto the first node and off the last.
    lead: Option<Joint>,
    trail: Option<Joint>,

    /// Set for a path held as its totals rather than its nodes.
    summary: Option<Summary>,
}

/// The totals of a path: see [`MapPath::summarised`].
#[derive(Clone, Copy, Debug)]
struct Summary {
    length: f64,
    complexity: f64,
    span: f64,
}

impl<E> From<Vec<Node<E>>> for MapPath<E>
where
    E: Entry,
{
    fn from(nodes: Vec<Node<E>>) -> Self {
        MapPath {
            nodes,
            strides: None,
            lead: None,
            trail: None,
            summary: None,
        }
    }
}

impl<E> MapPath<E>
where
    E: Entry,
{
    pub fn new(nodes: impl IntoIterator<Item = Node<E>>) -> Self {
        Self::from(nodes.into_iter().collect::<Vec<_>>())
    }

    /// A path whose geometry is already known: `strides[i]` is the segment
    /// from `nodes[i]` to `nodes[i + 1]`.
    pub(crate) fn with_geometry(
        nodes: Vec<Node<E>>,
        strides: Vec<Stride>,
        length: f64,
        lead: Option<Joint>,
        trail: Option<Joint>,
    ) -> Self {
        debug_assert_eq!(strides.len(), nodes.len().saturating_sub(1));

        MapPath {
            nodes,
            strides: Some((strides, length)),
            lead,
            trail,
            summary: None,
        }
    }

    /// A path held as its totals, with no nodes: its `length`, its angular
    /// `complexity` between the candidates it joins, and the straight-line
    /// `span` between them.
    pub(crate) fn summarised(length: f64, complexity: f64, span: f64) -> Self {
        MapPath {
            summary: Some(Summary {
                length,
                complexity,
                span,
            }),
            ..MapPath::from(Vec::new())
        }
    }

    /// The straight-line distance between the candidates the path joins,
    /// where it is [held](Self::summarised).
    pub(crate) fn span(&self) -> Option<f64> {
        self.summary.map(|summary| summary.span)
    }

    /// Converts a trip into a [`LineString`].
    pub fn linestring(&self) -> LineString {
        self.nodes
            .iter()
            .map(|v| v.position)
            .collect::<LineString>()
    }

    // TODO: This should be done lazily, since we may not need the points but possibly OK as is.
    /// Creates a new path from a slice of node ids, and a map to look up their locations.
    pub fn new_with_map<N: Network<Entry = E> + ?Sized>(map: &N, nodes: &[E]) -> Self {
        let resolved = map.line(nodes);

        let nodes = resolved
            .into_iter()
            .zip(nodes)
            .map(|(point, id)| Node::new(point, *id));

        MapPath::new(nodes)
    }

    /// Computes the angle between each pair of nodes in the trip.
    /// Allows you to understand the change in heading, aggregatable
    /// using [`MapPath::total_angle`] to determine the total variation
    /// exhibited during a trip.
    ///
    /// The returned vector will therefore have a length one less
    /// than the nodes given, and will be empty for a singular node
    /// as there is no delta exhibited.
    ///
    /// This assumes points are given on a great-circle, and uses
    /// Haversine comparisons.
    ///
    /// ### Example
    /// ```rust
    ///  use routers_codec::osm::element::variants::{OsmEntryId};
    ///  use routers_network::Node;
    ///  use routers_transition::MapPath;
    ///  use geo::Point;
    ///
    ///  // Create some nodes
    ///  let nodes: Vec<Node<OsmEntryId>> = vec![
    ///     Node::new(Point::new(0.0, 0.0), OsmEntryId::null()),
    ///     Node::new(Point::new(0.0, 1.0), OsmEntryId::null()),
    ///     Node::new(Point::new(1.0, 1.0), OsmEntryId::null()),
    ///     Node::new(Point::new(1.0, 0.0), OsmEntryId::null()),
    ///  ];
    ///
    ///  // Form a trip from these nodes
    ///  let trip = MapPath::from(nodes);
    ///
    ///  // Calculate the delta angle exhibited
    ///  println!("{:?}", trip.delta_angle());
    ///  // # [0, 90, 180]
    /// ```
    pub fn delta_angle(&self) -> Vec<f64> {
        Self::deltas_from_headings(&self.headings())
    }

    fn deltas_from_headings(headings: &[f64]) -> Vec<f64> {
        headings
            .windows(2)
            .map(|bearings| match bearings {
                [prev, curr] => heading_delta(*prev, *curr),
                _ => 0.0,
            })
            .collect()
    }

    /// Computes the bearing (heading) between each pair of consecutive positions in the list.
    ///
    /// The bearing is calculated using the haversine formula and represents the direction from the
    /// first point to the second, measured in degrees relative to due north (0°).
    ///
    /// Returns a vector of bearings, where each entry corresponds to the bearing between two
    /// consecutive positions in the list. If the input has fewer than 2 elements, the result will
    /// be an empty vector.
    ///
    /// # Returns
    ///
    /// A `Vec<f64>` where each element is the bearing in degrees between two consecutive positions.
    ///
    /// # Example
    /// ```
    /// use geo::Point;
    /// use routers_codec::osm::OsmEntryId;
    /// use routers_network::Node;
    /// use routers_transition::MapPath;
    ///
    /// let positions = vec![
    ///     // San Francisco (SF)
    ///     Node::new(Point::new(-122.4194, 37.7749), OsmEntryId::null()),
    ///     // Los Angeles (LA)
    ///     Node::new(Point::new(-118.2437, 34.0522), OsmEntryId::null()),
    ///     // Las Vegas (LV)
    ///     Node::new(Point::new(-115.1398, 36.1699), OsmEntryId::null()),
    /// ];
    ///
    /// // [heading SF → LA, heading LA → LV]
    /// MapPath::from(positions).headings();
    /// ```
    pub fn headings(&self) -> Vec<f64> {
        Self::headings_from_positions(self.nodes.iter().map(|n| n.position))
    }

    fn headings_from_positions(positions: impl IntoIterator<Item = Point>) -> Vec<f64> {
        let pts: Vec<Point> = positions.into_iter().collect();
        pts.windows(2)
            .filter_map(|pair| match pair {
                [a, b] => {
                    // Bearing is undefined for overlapping points.
                    if Haversine.distance(*a, *b) < 1.0 {
                        None
                    } else {
                        Some(Haversine.bearing(*a, *b))
                    }
                }
                _ => None,
            })
            .collect()
    }

    /// Computes the sum of angle differences within a trip.
    /// Useful as a quantifiable heuristic to determine how "non-direct" a trip is.
    ///
    /// ### Example
    /// ```rust
    ///  use routers_codec::osm::element::variants::{OsmEntryId};
    ///  use routers_network::Node;
    ///  use routers_transition::MapPath;
    ///  use geo::Point;
    ///
    ///  // Create some nodes
    ///  let nodes: Vec<Node<OsmEntryId>> = vec![
    ///     Node::new(Point::new(0.0, 0.0), OsmEntryId::null()),
    ///     Node::new(Point::new(0.0, 1.0), OsmEntryId::null()),
    ///     Node::new(Point::new(1.0, 1.0), OsmEntryId::null()),
    ///     Node::new(Point::new(1.0, 0.0), OsmEntryId::null()),
    ///  ];
    ///
    ///  // Form a trip from these nodes
    ///  let trip = MapPath::from(nodes);
    ///
    ///  // Calculate the total angle exhibited
    ///  println!("{}", trip.total_angle());
    ///  // # 180
    /// ```
    pub fn total_angle(&self) -> f64 {
        self.delta_angle().into_iter().map(|v| v.abs()).sum()
    }

    /// Calculates the "immediate" (or average) angle within a trip.
    /// Used to understand the "average" angular movement per move.
    ///
    /// It is important to understand this is intrinsically weighted
    /// by the number of nodes, such that denser areas will reduce
    /// this weighting and vice versa.
    pub fn immediate_angle(&self) -> f64 {
        self.total_angle() / (self.nodes.len() as f64)
    }

    /// Describes the angle experienced as a representation of the immediate angle
    /// over the distance travelled. Therefore, meaning it can be used to compare
    /// the angles of two trips on a given distance to understand which one had
    /// more turning.
    ///
    /// The distance parameter provided is used to grade complexity against a constant
    /// heuristic. The distance is used to emulate a "worst traversal" across the distance
    /// such that the provided trip can be compared to have been better or worse than
    /// this theoretically worst trip.
    ///
    /// ### Example
    ///
    /// As an example, [`DefaultTransitionCost`](crate::DefaultTransitionCost)
    /// uses this heuristic to grade the trip
    /// between two candidates against the distance between the candidates, `d`.
    ///
    /// The trips themselves have distances `d1`, `d2`, `d3`, and so on. These values are not `d`,
    /// but can be graded against `d` as a common distance such that the heuristic can
    /// understand if the trip taken between the two nodes is theoretically simple or
    /// theoretically complex.
    pub fn angular_complexity(&self) -> f64 {
        Self::complexity_from_deltas(self.delta_angle())
    }

    /// Like [`angular_complexity`](Self::angular_complexity), but evaluates the trip with
    /// `source` prepended and `target` appended. Used to account for the turn induced at
    /// the candidate→edge-endpoint joints of an intra-transition.
    pub fn angular_complexity_with_endpoints(&self, source: Point, target: Point) -> f64 {
        let positions = core::iter::once(source)
            .chain(self.nodes.iter().map(|n| n.position))
            .chain(core::iter::once(target));
        let headings = Self::headings_from_positions(positions);
        Self::complexity_from_deltas(Self::deltas_from_headings(&headings))
    }

    /// Like [`angular_complexity_with_endpoints`](Self::angular_complexity_with_endpoints), but also
    /// explicitly includes the required initial and final vehicle headings at the source and target candidates.
    pub fn angular_complexity_with_headings(
        &self,
        start_heading: Option<f64>,
        end_heading: Option<f64>,
        source: Point,
        target: Point,
    ) -> f64 {
        if let Some(summary) = self.summary {
            return summary.complexity;
        }

        match (self.nodes.first(), self.nodes.last()) {
            (Some(first), Some(last)) => {
                let (lead, onto) = match self.lead {
                    Some(joint) if joint.position == source => (joint.segment, joint.turn),
                    _ => (Segment::between(source, first.position), None),
                };
                let mut turns = Turns::leaving(start_heading, lead, onto);

                match &self.strides {
                    Some((strides, _)) => turns.push_strides(strides),
                    None => self.nodes.windows(2).for_each(|pair| {
                        if let [a, b] = pair {
                            turns.push_segment(Segment::between(a.position, b.position));
                        }
                    }),
                }

                let (trail, off) = match self.trail {
                    Some(joint) if joint.position == target => (joint.segment, joint.turn),
                    _ => (Segment::between(last.position, target), None),
                };

                turns.arriving(trail, off, end_heading)
            }
            _ => {
                let mut turns = Turns::default();

                if let Some(heading) = start_heading {
                    turns.push(heading);
                }
                turns.push_segment(Segment::between(source, target));
                if let Some(heading) = end_heading {
                    turns.push(heading);
                }

                turns.complexity()
            }
        }
    }

    fn complexity_from_deltas(angles: Vec<f64>) -> f64 {
        let length = angles.len();
        if length == 0 {
            return 1.0;
        }

        let costs: Vec<f64> = angles.into_iter().map(turn_cost).collect();

        // Any zero contribution (turns ≥ 112.5°) makes the whole path cost zero.
        // Harmonic mean is used for non-zero segments: it penalises the worst turns
        // more strongly than arithmetic mean, giving better A* discrimination.
        if costs.iter().any(|&c| c <= 0.0) {
            return 0.0;
        }

        let harmonic_sum: f64 = costs.iter().map(|&c| 1.0 / c).sum();
        (length as f64 / harmonic_sum).clamp(0.0, 1.0)
    }

    /// Returns the length of the trip in meters, calculated
    /// by the cumulative distance between each entry in the trip.
    pub fn length(&self) -> f64 {
        if let Some(summary) = self.summary {
            return summary.length;
        }

        if let Some((_, length)) = self.strides {
            return length;
        }

        self.nodes.windows(2).fold(0.0, |length, node| {
            if let [a, b] = node {
                return length + Haversine.distance(a.position, b.position);
            }

            length
        })
    }

    pub fn straightline_length(&self) -> f64 {
        let start = self.nodes.first();
        let end = self.nodes.last();

        if let (Some(start), Some(end)) = (start, end) {
            return Haversine.distance(start.position, end.position);
        }

        0.0
    }
}

/// The signed change in heading from `prev` to `curr`, in `(-180, +180]`.
#[inline]
fn heading_delta(prev: f64, curr: f64) -> f64 {
    // Bearings lie in `[0, 360)`, so their difference is its own remainder.
    let delta = curr - prev;
    let delta = if delta.abs() < 360.0 {
        delta
    } else {
        wound(delta)
    };
    if delta > 180.0 {
        delta - 360.0
    } else if delta <= -180.0 {
        delta + 360.0
    } else {
        delta
    }
}

/// Out of line: inline, the remainder is computed unconditionally.
#[cold]
#[inline(never)]
fn wound(delta: f64) -> f64 {
    delta.rem(360.0)
}

/// The cost of a turn of `angle` degrees: `1` straight on, `0` from 112.5°.
#[inline]
fn turn_cost(angle: f64) -> f64 {
    // The maximum knowable rotation in differential angles is between [-180, +180].
    const MAX_ANGLE: f64 = 180.0;
    // Compresses the angle range so that turns ≥ 112.5° map to cos ≤ 0 → clamped to 0.
    const COST_DAMPING: f64 = 0.8;

    let angle = angle.clamp(-MAX_ANGLE, MAX_ANGLE);
    (angle.mul(PI).div(MAX_ANGLE).mul(COST_DAMPING).cos()).clamp(0.0, 1.0)
}

/// Streaming form of [`MapPath::complexity_from_deltas`]: the same harmonic
/// mean of turn costs, fed one heading at a time.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Turns {
    previous: Option<f64>,
    /// Whether `previous` is the heading of a [`Stride`].
    striding: bool,
    count: usize,
    harmonic_sum: f64,
    blocked: bool,
}

impl Turns {
    /// The turns made on the way onto a path, via the `lead` joint; `onto` is
    /// that turn, where it has been measured.
    #[inline]
    pub(crate) fn leaving(start_heading: Option<f64>, lead: Segment, onto: Option<Turn>) -> Self {
        let mut turns = Self::default();

        if let Some(heading) = start_heading {
            turns.push(heading);
        }
        if let Some(heading) = lead.heading() {
            turns.push_turn(heading, onto);
        }

        turns
    }

    /// These turns, and then the one onto `stride`.
    #[inline]
    pub(crate) fn along(mut self, stride: Stride) -> Self {
        self.push_stride(stride);
        self
    }

    /// The complexity of the whole path, once it has left by the `trail` and
    /// turned (`off`, where measured) onto the edge it ends on.
    #[inline(always)]
    pub(crate) fn arriving(
        mut self,
        trail: Segment,
        off: Option<Turn>,
        end_heading: Option<f64>,
    ) -> f64 {
        self.push_segment(trail);

        if let Some(heading) = end_heading {
            self.push_turn(heading, off);
        }

        self.complexity()
    }

    #[inline]
    fn push_segment(&mut self, segment: Segment) {
        if let Some(heading) = segment.heading() {
            self.push(heading);
        }
    }

    /// As [`push_segment`](Self::push_segment), with the stride's measured turn.
    #[inline]
    fn push_stride(&mut self, stride: Stride) {
        let Some(heading) = stride.segment.heading() else {
            return;
        };

        match stride.turn {
            Some(reciprocal) if self.striding => {
                self.previous = Some(heading);
                self.count += 1;
                self.blocked |= reciprocal == BLOCKED;
                self.harmonic_sum += reciprocal;
            }
            _ => self.push(heading),
        }
        self.striding = true;
    }

    /// [`push_stride`](Self::push_stride) over a run of strides.
    fn push_strides(&mut self, strides: &[Stride]) {
        let mut strides = strides.iter();

        for stride in strides.by_ref() {
            self.push_stride(*stride);
            if self.striding {
                break;
            }
        }

        let (mut previous, mut count) = (self.previous, self.count);
        let (mut sum, mut blocked) = (self.harmonic_sum, self.blocked);

        for stride in strides {
            match (stride.segment.heading(), stride.turn) {
                (None, _) => {}
                (Some(heading), Some(reciprocal)) => {
                    previous = Some(heading);
                    count += 1;
                    blocked |= reciprocal == BLOCKED;
                    sum += reciprocal;
                }
                (Some(heading), None) => {
                    // Unmeasured: take the turn the long way.
                    (self.previous, self.count) = (previous, count);
                    (self.harmonic_sum, self.blocked) = (sum, blocked);
                    self.push(heading);
                    self.striding = true;
                    (previous, count) = (self.previous, self.count);
                    (sum, blocked) = (self.harmonic_sum, self.blocked);
                }
            }
        }

        (self.previous, self.count) = (previous, count);
        (self.harmonic_sum, self.blocked) = (sum, blocked);
    }

    #[inline]
    fn push(&mut self, heading: f64) {
        self.push_turn(heading, None);
    }

    /// As [`push`](Self::push), using `measured` if that is the turn made.
    #[inline]
    fn push_turn(&mut self, heading: f64, measured: Option<Turn>) {
        self.striding = false;

        if let Some(previous) = self.previous.replace(heading) {
            let cost = match measured {
                Some(turn) if turn.from == previous && turn.to == heading => turn.cost,
                _ => turn_cost(heading_delta(previous, heading)),
            };
            self.turn(cost);
        }
    }

    #[inline]
    fn turn(&mut self, cost: f64) {
        self.count += 1;
        self.blocked |= cost <= 0.0;
        self.harmonic_sum += 1.0 / cost;
    }

    fn complexity(&self) -> f64 {
        if self.count == 0 {
            return 1.0;
        }
        if self.blocked {
            return 0.0;
        }

        (self.count as f64 / self.harmonic_sum).clamp(0.0, 1.0)
    }
}
