use crate::transition::primitives::Fraction;

use core::cmp::Ordering;
use core::ops::Add;
use pathfinding::num_traits::Zero;

/// Represents a thin structure storing the weight and distance associated with a candidate
#[allow(clippy::derived_hash_with_manual_eq)]
#[derive(Copy, Clone, Hash, Debug)]
pub struct WeightAndDistance(pub Fraction, pub u32);

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
        let w = self.0.value() as f64;
        (w * w * self.1 as f64) as u32
    }

    #[inline]
    pub const fn new(frac: Fraction, weight: u32) -> Self {
        Self(frac, weight)
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
        WeightAndDistance(self.0 + rhs.0, self.1 + rhs.1)
    }
}

impl Zero for WeightAndDistance {
    fn zero() -> Self {
        WeightAndDistance(Fraction::zero(), 0)
    }

    fn is_zero(&self) -> bool {
        self.repr() == 0
    }
}
