use crate::Graph;
use geo::{Bearing, Distance, Haversine, LineString};
use routers_codec::primitive::Node;
use routers_codec::{Entry, Metadata};

/// Utilities to calculate metadata of a trip.
/// A trip is composed of a collection of [`Node`] entries.
///
/// These entries contain positioning data which are used
/// to provide utilities such as conversions into a [`LineString`],
/// finding the total travelled angle, and finding the trips summative length.
#[derive(Clone, Debug)]
pub struct Trip<E>(Vec<Node<E>>)
where
    E: Entry;

impl<E> From<Vec<Node<E>>> for Trip<E>
where
    E: Entry,
{
    fn from(nodes: Vec<Node<E>>) -> Self {
        Trip(nodes)
    }
}

impl<E> Trip<E>
where
    E: Entry,
{
    pub fn new(nodes: impl IntoIterator<Item = Node<E>>) -> Self {
        Self(nodes.into_iter().collect::<Vec<_>>())
    }

    /// Converts a trip into a [`LineString`].
    pub(crate) fn linestring(&self) -> LineString {
        self.0.iter().map(|v| v.position).collect::<LineString>()
    }

    // TODO: This should be done lazily, since we may not need the points but possibly OK as is.
    /// Creates a new trip from a slice of [`NodeIx`]s, and a map to lookup their location.
    pub fn new_with_map<M: Metadata>(map: &Graph<E, M>, nodes: &[E]) -> Self {
        let resolved = map.get_line(nodes);

        let nodes = resolved
            .into_iter()
            .zip(nodes)
            .map(|(point, id)| Node::new(point, *id));

        Trip::new(nodes)
    }

    /// Computes the angle between each pair of nodes in the trip.
    /// Allows you to understand the change in heading, aggregatable
    /// using [`Trip::total_angle`] to determine the total variation
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
    ///  use routers_codec::primitive::Node;
    ///  use routers::transition::Trip;
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
    ///  let trip = Trip::from(nodes);
    ///
    ///  // Calculate the delta angle exhibited
    ///  println!("{:?}", trip.delta_angle());
    ///  // # [0, 90, 180]
    /// ```
    pub fn delta_angle(&self) -> Vec<f64> {
        self.headings()
            .windows(2)
            .map(|bearings| {
                if let [prev, curr] = bearings {
                    let mut turn_angle = (curr - prev).abs();

                    // Normalize to [-180, 180] degrees
                    if turn_angle > 180.0 {
                        turn_angle -= 360.0;
                    } else if turn_angle < -180.0 {
                        turn_angle += 360.0;
                    }

                    turn_angle.abs()
                } else {
                    0.0
                }
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
    /// use routers_codec::primitive::Node;
    /// use routers::transition::Trip;
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
    /// Trip::from(positions).headings();
    /// ```
    pub fn headings(&self) -> Vec<f64> {
        self.0
            .windows(2)
            .filter_map(|entries| {
                if let [a, b] = entries {
                    // Because bearing cannot be calculated for overlapping nodes
                    if Haversine.distance(a.position, b.position) < 1.0 {
                        return None;
                    }

                    // Returns the bearing relative to due-north
                    Some(Haversine.bearing(a.position, b.position))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    }

    /// Computes the sum of angle differences within a trip.
    /// Useful as a quantifiable heuristic to determine how "non-direct" a trip is.
    ///
    /// ### Example
    /// ```rust
    ///  use routers_codec::osm::element::variants::{OsmEntryId};
    ///  use routers_codec::primitive::Node;
    ///  use routers::transition::Trip;
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
    ///  let trip = Trip::from(nodes);
    ///
    ///  // Calculate the total angle exhibited
    ///  println!("{}", trip.total_angle());
    ///  // # 180
    /// ```
    pub fn total_angle(&self) -> f64 {
        self.delta_angle().into_iter().sum()
    }

    /// Calculates the "immediate" (or average) angle within a trip.
    /// Used to understand the "average" angular movement per move.
    ///
    /// It is important to understand this is intrinsically weighted
    /// by the number of nodes, such that denser areas will reduce
    /// this weighting and vice versa.
    pub fn immediate_angle(&self) -> f64 {
        self.total_angle() / (self.0.len() as f64)
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
    /// As an example [`DefaultTransitionCost`], uses this heuristic to grade the trip
    /// between two candidates against the distance between the candidates, `d`.
    ///
    /// The trips themselves have distances `d1`, `d2`, `d3`, and so on. These values are not `d`,
    /// but can be graded against `d` as a common distance such that the heuristic can
    /// understand if the trip taken between the two nodes is theoretically simple or
    /// theoretically complex.
    pub fn angular_complexity(&self, distance: f64) -> f64 {
        const U_TURN: f64 = 179.;
        const DIST_BETWEEN_ZIGZAG: f64 = 100.0; // 100m minimum
        const ZIG_ZAG: f64 = 180.0;

        // At least 1
        let num_zig_zags: f64 = (distance / DIST_BETWEEN_ZIGZAG).max(1.);

        let sum = self.total_angle();
        if self.delta_angle().iter().any(|v| v.abs() >= U_TURN) {
            // Should not take this path - but does not exclude it incase it's the only option.
            return 0.0;
        }

        // Complete Zig-Zag
        let theoretical_max = num_zig_zags * ZIG_ZAG;

        // Sqrt used to create "stretch" to optimality.
        1.0 - (sum / theoretical_max).sqrt().clamp(0.0, 1.0)
    }

    /// Returns the length of the trip in meters, calculated
    /// by the cumulative distance between each entry in the trip.
    pub fn length(&self) -> f64 {
        self.0.windows(2).fold(0.0, |length, node| {
            if let [a, b] = node {
                return length + Haversine.distance(a.position, b.position);
            }

            length
        })
    }
}
