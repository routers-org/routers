use crate::*;
use routers_network::{Entry, Metadata, Network};

const PRECISION: f64 = 100.0f64;

pub trait Strategy<Ctx> {
    /// A calculable cost which can be any required
    /// type, so long as it is castable into a 64-bit float.
    type Cost: Into<f64>;

    /// The zeta (ζ) value in the decay function.
    const ZETA: f64;

    /// The beta (β) value in the decay function.
    const BETA: f64;

    /// Returned values must be in the range `[0, 1]`, where `1` represents a
    /// perfect (free) choice and `0` the most expensive possible cost.
    fn calculate(&self, context: Ctx) -> Option<Self::Cost>;

    /// Converts the `[0, 1]` output of [`calculate`] to a `u32` edge cost.
    ///
    /// ```math
    /// cost(v) = ζ · (1/v)^β · PRECISION
    /// ```
    ///
    /// [`calculate`]: Strategy::calculate
    #[inline(always)]
    fn cost(&self, ctx: Ctx) -> u32 {
        const EPSILON: f64 = 1e-6;

        let v = self
            .calculate(ctx)
            .map_or(0.0, |v| v.into())
            .clamp(EPSILON, 1.0);

        let cost = (1.0 / v).powf(Self::BETA);
        (PRECISION * Self::ZETA * cost) as u32
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

    /// The transition costing function, returning a u32 cost value.
    fn transition(&self, context: TransitionContext<E, M, N>) -> u32;
}
