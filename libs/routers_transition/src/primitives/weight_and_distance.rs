use core::cmp::Ordering;
use core::ops::Add;
use num_traits::Zero;
use routers_network::edge::Weight;
use uom::si::f64::Length;
use uom::si::length::centimeter;

/// The accumulated routing cost of a candidate path.
///
/// It carries a running average road-class weight — held as a separate
/// `numerator` (sum of weights) and `denominator` (number of edges) so the
/// average stays exact under addition — alongside the cumulative distance
/// travelled.
///
/// Distance crosses the API as a [`Length`], but is held in whole centimetres
/// so accumulation over a long path stays exact where repeated
/// floating-point addition would drift. The field names its unit because
/// nothing in the type does.
#[allow(clippy::derived_hash_with_manual_eq)]
#[derive(Copy, Clone, Hash, Debug)]
pub struct WeightAndDistance {
    numerator: Weight,
    denominator: u32,
    distance_cm: u32,
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
        (self.squared_weight() * f64::from(self.distance_cm)) as u32
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

    /// The cumulative distance travelled.
    #[inline]
    pub fn distance(&self) -> Length {
        Length::new::<centimeter>(f64::from(self.distance_cm))
    }

    /// Constructs the cost of a single edge of the given road-class `weight`
    /// and `distance`.
    #[inline]
    pub fn new(weight: Weight, distance: Length) -> Self {
        Self {
            numerator: weight,
            denominator: 1,
            distance_cm: distance.get::<centimeter>() as u32,
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
            distance_cm: self.distance_cm + rhs.distance_cm,
        }
    }
}

impl Zero for WeightAndDistance {
    fn zero() -> Self {
        WeightAndDistance {
            numerator: 0,
            denominator: 0,
            distance_cm: 0,
        }
    }

    fn is_zero(&self) -> bool {
        self.repr() == 0
    }
}
