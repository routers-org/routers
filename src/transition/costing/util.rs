use crate::transition::*;
use routers_codec::{Entry, Metadata};
use std::f64::consts::E;

const PRECISION: f64 = 1_000.0f64;
const OFFSET: f64 = E;

pub trait Strategy<Ctx> {
    /// A calculable cost which can be any required
    /// type, so long as it is castable into a 64-bit float.
    type Cost: Into<f64>;

    /// The zeta (ζ) value in the decay function.
    const ZETA: f64;

    /// The beta (β) value in the decay function.
    const BETA: f64;

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
    fn cost(&self, ctx: Ctx) -> u32 {
        // The base multiplier (1 / ζ)
        let multiplier = 1.0 / Self::ZETA;

        // The exponential cost heuristic (-1 * value / β)
        let cost = -self.calculate(ctx).map_or(f64::INFINITY, |v| v.into()) / Self::BETA;

        // Shift so low-costs have low output costs (normalised)
        let shifted = ((multiplier * cost.exp()) - OFFSET).max(0.);

        // Since output must be `u32`, we shift by `PRECISION` to
        // increase the cost precision.
        //
        // Note: This must be replicated for all cost heuristics since
        //       this will determine the overall magnitude of costs.
        (PRECISION * shifted) as u32
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
    fn emission(&self, context: EmissionContext) -> u32;

    /// The emission costing function, returning a u32 cost value.
    fn transition(&self, context: TransitionContext<E, M>) -> u32;
}
