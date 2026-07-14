use core::cmp::Ordering;
use core::ops::Add;
use pathfinding::num_traits::Zero;
use routers_network::edge::Weight;

/// The accumulated routing cost of a candidate path.
///
/// It carries a running average road-class weight — held as a separate
/// `numerator` (sum of weights) and `denominator` (number of edges) so the
/// average stays exact under addition — alongside the cumulative `distance`
/// travelled, in centimeters.
#[allow(clippy::derived_hash_with_manual_eq)]
#[derive(Copy, Clone, Hash, Debug)]
pub struct WeightAndDistance {
    numerator: Weight,
    denominator: u32,
    distance: u32,
}

impl WeightAndDistance {
    /// A representation method which allows distinguishment between structures
    /// on a given `f(weight, distance) = weight² × distance` function,
    /// returning a `u32` representation of the structure.
    ///
    /// Using a quadratic road-class weighting ensures that the Dijkstra path
    /// finder strongly penalises lower-quality roads (e.g. offramps /
    /// MotorwayLink), preventing short detours through lower-class roads from
    /// being preferred over longer, same-class routes.
    ///
    /// With quadratic weighting a MotorwayLink detour (weight=2) has an
    /// effective cost 4× that of an equal-length motorway segment (weight=1),
    /// so the direct motorway is preferred unless the detour is less than
    /// one quarter of the motorway path length.
    #[inline]
    pub fn repr(&self) -> u32 {
        (self.squared_weight() * self.distance()) as u32
    }

    /// The running average road-class weight (numerator / denominator).
    #[inline]
    const fn weight(&self) -> Weight {
        if self.denominator == 0 {
            return 0;
        }

        self.numerator / self.denominator
    }

    #[inline]
    fn squared_weight(&self) -> f64 {
        (self.weight() as f64).powi(2)
    }

    #[inline]
    const fn distance(&self) -> f64 {
        self.distance as f64
    }

    /// The cumulative distance travelled, in centimeters.
    #[inline]
    pub const fn distance_cm(&self) -> u32 {
        self.distance
    }

    /// Constructs the cost of a single edge of the given road-class `weight`
    /// and `distance` (in centimeters).
    #[inline]
    pub const fn new(weight: Weight, distance: u32) -> Self {
        Self {
            numerator: weight,
            denominator: 1,
            distance,
        }
    }
}

impl Eq for WeightAndDistance {}

impl PartialEq<Self> for WeightAndDistance {
    fn eq(&self, other: &Self) -> bool {
        self.repr() == other.repr()
    }
}

impl PartialOrd for WeightAndDistance {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WeightAndDistance {
    fn cmp(&self, other: &Self) -> Ordering {
        self.repr().cmp(&other.repr())
    }
}

impl Add<Self> for WeightAndDistance {
    type Output = WeightAndDistance;

    fn add(self, rhs: Self) -> Self::Output {
        WeightAndDistance {
            numerator: self.numerator + rhs.numerator,
            denominator: self.denominator + rhs.denominator,
            distance: self.distance + rhs.distance,
        }
    }
}

impl Zero for WeightAndDistance {
    fn zero() -> Self {
        WeightAndDistance {
            numerator: 0,
            denominator: 0,
            distance: 0,
        }
    }

    fn is_zero(&self) -> bool {
        self.repr() == 0
    }
}
