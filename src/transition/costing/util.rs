use crate::transition::*;
use routers_codec::{Entry, Metadata};

pub trait Strategy<Ctx> {
    /// A calculable cost which can be any required
    /// type, so long as it is castable into a 64-bit float.
    type Cost: Into<f64>;

    /// The calculation cost you must implement
    fn calculate(&self, context: Ctx) -> Option<Self::Cost>;

    /// An optimal decay-based costing heuristic which accepts
    /// the input value and transforms it using the associated
    /// constants `ZETA` and `BETA` to calculate the resultant output
    /// cost using the `decay` method.
    ///
    /// ### Formula
    /// The scalar is given by `1 / ζ`. Therefore, if `ζ` is `1`, no
    /// scaling is applied. The exponential component is the negative
    /// value divided by `β`. The absolute value of the resultant is taken.
    ///
    /// ```math
    /// decay(value) = |(1 / ζ) * e^(-1 * value / β)| - offset
    /// ```
    #[inline(always)]
    fn cost(&self, ctx: Ctx) -> f64 {
        const IMPOSSIBLE_ROUTE: f64 = 0.;
        self.calculate(ctx).map_or(IMPOSSIBLE_ROUTE, |v| v.into())
    }
}

pub trait Costing<Emission, Transition, E, M>
where
    E: Entry,
    M: Metadata,
    Transition: TransitionStrategy<E, M>,
    Emission: EmissionStrategy,
{
    /// The emission costing function, returning a u32 cost value.
    fn emission(&self, context: EmissionContext) -> f64;

    /// The emission costing function, returning a u32 cost value.
    fn transition(&self, context: TransitionContext<E, M>) -> f64;
}
