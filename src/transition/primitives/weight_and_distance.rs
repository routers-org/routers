use crate::transition::primitives::Fraction;

use pathfinding::num_traits::Zero;
use std::cmp::Ordering;
use std::ops::Add;

/// Represents a thin structure storing the weight and distance associated with a candidate
#[allow(clippy::derived_hash_with_manual_eq)]
#[derive(Copy, Clone, Hash, Debug)]
pub struct WeightAndDistance(pub Fraction, pub u32);

impl WeightAndDistance {
    /// A representation method which allows distinguishment between structures
    /// on a given `f(weight, distance) = sqrt(weight) * distance` function,
    /// returning a `u32` representation of the structure.
    #[inline]
    pub fn repr(&self) -> u32 {
        ((self.0.value() as f64).sqrt() * self.1 as f64) as u32
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
