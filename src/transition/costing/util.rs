use crate::transition::*;
use routers_network::{Entry, Metadata, Network};
use std::ops::Mul;

const PRECISION: f64 = 100.0f64;

pub trait Strategy<Ctx> {
    /// A calculable cost which can be any required
    /// type, so long as it is castable into a 64-bit float.
    type Cost: Into<f64>;

    /// The zeta (ζ) value in the decay function.
    const ZETA: f64;

    /// The beta (β) value in the decay function.
    const BETA: f64;

    /// The calculation cost you must implement.
    /// Returned values must be in the range [0, 1], inclusive.
    ///
    /// 1 represents a perfect cost, as if the choice were free.
    /// 0 represents the most expensive possible cost.
    fn calculate(&self, context: Ctx) -> Option<Self::Cost>;

    /// Converts the raw `[0, 1]` output of [`calculate`] to a `u32` edge cost
    /// via the reciprocal-minus-one transform, scaled by `PRECISION`.
    ///
    /// ### Formula
    ///
    /// ```math
    /// cost(v) = max(0, 1/(β·v) - 1) × PRECISION
    /// ```
    ///
    /// This grows hyperbolically as `v → 0`, giving a ~25× cost ratio between
    /// the best and worst plausible transitions — enough for A* to prune
    /// aggressively. The earlier `-ln(β·v)` formula only produced a ~2× ratio
    /// for urban routes where all road weights are > 1, which caused the
    /// selective solver to explore the full candidate graph.
    ///
    /// [`calculate`]: Strategy::calculate
    #[inline(always)]
    fn cost(&self, ctx: Ctx) -> u32 {
        // Maps calculate()'s [0, 1] output to [0, ∞) via 1/(β·v) - 1.
        //
        // This grows much faster than -ln(v) as v → 0, giving the A* solver
        // the discrimination it needs to prune bad transitions early. With
        // -ln(v) all urban transitions cluster within a 2× band; with 1/v-1
        // the best-to-worst ratio is ~25× for the same inputs.
        //
        // None from calculate() maps v = 0 → cost = u32::MAX (reject path).
        let cost = (self
            .calculate(ctx)
            .map_or(0.0, |v| v.into())
            .mul(Self::BETA)
            .recip()
            - 1.0)
            .max(0.);

        // Since output must be `u32`, we shift by `PRECISION` to
        // increase the cost precision.
        //
        // Note: This must be replicated for all cost heuristics since
        //       this will determine the overall magnitude of costs.
        (PRECISION * cost) as u32
    }
}

pub trait Costing<Emission, Transition, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Transition: TransitionStrategy<E, M, N>,
    Emission: EmissionStrategy,
{
    /// The emission costing function, returning a u32 cost value.
    fn emission(&self, context: EmissionContext) -> u32;

    /// The emission costing function, returning a u32 cost value.
    fn transition(&self, context: TransitionContext<E, M, N>) -> u32;
}
