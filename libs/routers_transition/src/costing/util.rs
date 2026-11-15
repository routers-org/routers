use crate::costing::{
    EmissionContext, EmissionStrategy, HopSummary, TransitionContext, TransitionStrategy,
};
use routers_network::Entry;

const PRECISION: f64 = 100.0f64;

pub trait Strategy<Ctx> {
    /// A calculable cost which can be any required
    /// type, so long as it is castable into a 64-bit float.
    type Cost: Into<f64>;

    /// The zeta (ζ) value in the decay function.
    const ZETA: f64;

    /// The beta (β) value in the decay function.
    const BETA: f64;

    /// Whether this strategy needs the routed path itself. One that sets this
    /// `false` is asked to cost each routed hop through
    /// [`summarised`](Strategy::summarised) instead.
    const WALKS_PATH: bool = true;

    /// [`calculate`](Strategy::calculate), for a routed hop known only by
    /// its [`HopSummary`]. Must be implemented by a strategy that sets
    /// [`WALKS_PATH`](Strategy::WALKS_PATH) to `false`; the default panics.
    fn summarised(&self, _hop: &HopSummary) -> Option<Self::Cost> {
        unimplemented!("a strategy with `WALKS_PATH = false` must implement `Strategy::summarised`")
    }

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
        Self::decay(self.calculate(ctx))
    }

    /// [`cost`](Strategy::cost), for a hop costed through
    /// [`summarised`](Strategy::summarised).
    #[inline(always)]
    fn summarised_cost(&self, hop: &HopSummary) -> u32 {
        Self::decay(self.summarised(hop))
    }

    /// The decay function: a `[0, 1]` value to its `u32` edge cost.
    #[inline(always)]
    fn decay(value: Option<Self::Cost>) -> u32 {
        const EPSILON: f64 = 1e-6;

        let v = value.map_or(0.0, |v| v.into()).clamp(EPSILON, 1.0);

        // `x.powf(1.0)` is exactly `x`; spares the default strategies a `pow`.
        let cost = if Self::BETA == 1.0 {
            1.0 / v
        } else {
            (1.0 / v).powf(Self::BETA)
        };
        (PRECISION * Self::ZETA * cost) as u32
    }
}

pub trait Costing<Emission, Transition, E>
where
    E: Entry,
    Transition: TransitionStrategy<E>,
    Emission: EmissionStrategy,
{
    /// The emission costing function, returning a u32 cost value.
    fn emission(&self, context: EmissionContext) -> u32;

    /// The transition costing function, returning a u32 cost value.
    fn transition(&self, context: TransitionContext<E>) -> u32;
}
