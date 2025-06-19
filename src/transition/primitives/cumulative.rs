use crate::graph::Weight;
use pathfinding::num_traits::Zero;
use std::ops::Add;

/// A fractional value, broken into a stored numerator and denominator.
/// It supports the [`Add`] trait wherein the combination of numerator
/// and denominator is supported.
///
/// The fractional value can be derived by the [`Fraction::value`] function.
#[derive(Copy, Clone, Hash, Debug)]
pub struct Fraction {
    pub numerator: Weight,
    pub denominator: u32,
}

impl Fraction {
    /// Returns the fractional value (num / denom) of the cumulative fraction.
    pub(crate) fn value(&self) -> Weight {
        if self.denominator == 0 {
            return 0;
        }

        self.numerator / self.denominator
    }

    #[inline]
    pub(crate) const fn mul(numerator: Weight) -> Self {
        Fraction {
            numerator,
            denominator: 1,
        }
    }
}

impl Zero for Fraction {
    fn zero() -> Self {
        Fraction {
            numerator: 0,
            denominator: 0,
        }
    }

    fn is_zero(&self) -> bool {
        self.value() == 0
    }
}

impl Add<Fraction> for Fraction {
    type Output = Fraction;

    fn add(self, rhs: Fraction) -> Self::Output {
        Fraction {
            numerator: self.numerator + rhs.numerator,
            denominator: self.denominator + rhs.denominator,
        }
    }
}
