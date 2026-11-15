pub mod emission {
    use core::ops::{Div, Neg};

    use crate::costing::{EmissionContext, Strategy};

    /// 5 meters (85th% GPS error)
    const DEFAULT_EMISSION_ERROR: f64 = 25.0;

    /// Calculates the emission cost of a candidate relative
    /// to its source node.
    ///
    /// ## Calculation
    ///
    /// The emission cost is defined by the  relative distance
    /// from the source position, given some "free" zone, known
    /// as the [`emission_error`](#field.emission_error). Within this error, any
    /// candidate is considered a no-cost transition as the likelihood
    /// the position is within the boundary is equal within this radius.
    ///
    /// The relative calculation is given simply below, where `distance`
    /// defines the haversine distancing function between two Lat/Lng positions.
    ///
    /// ```math
    /// relative(source, candidate) = err / distance(source, candidate)
    /// ```
    ///
    /// The cost derived is given as the square root of the reciprocal of
    /// the relative distance.
    ///
    /// ```math
    /// cost(source, candidate) = sqrt(1 / relative(source, candidate))
    /// ```
    ///
    /// There exist values within the strategy
    /// implementation which define how "aggressive" the falloff is.
    /// These hyperparameters may need to be tuned in order to calculate for nodes
    /// which have large error. Alternatively, providing your own emission error
    /// is possible too.
    pub struct DefaultEmissionCost {
        /// The free radius around which emissions cost the same, to provide
        /// equal opportunity to nodes within the expected GPS error.
        ///
        /// Default: 25 meters.
        pub emission_error: f64,
    }

    impl Default for DefaultEmissionCost {
        fn default() -> Self {
            DefaultEmissionCost {
                emission_error: DEFAULT_EMISSION_ERROR,
            }
        }
    }

    impl<'a> Strategy<EmissionContext<'a>> for DefaultEmissionCost {
        type Cost = f64;

        const ZETA: f64 = 1.;
        const BETA: f64 = 1.;

        #[inline(always)]
        fn calculate(&self, context: EmissionContext<'a>) -> Option<Self::Cost> {
            Some(context.distance.div(self.emission_error).sqrt().neg().exp())
        }
    }
}

pub mod transition {
    use crate::costing::{HopSummary, Strategy, TransitionContext, TransitionLengths};
    use routers_network::Entry;
    use routers_network::edge::Weight;

    /// Calculates the transition cost between two candidates.
    ///
    /// Involves the following "sub-heuristics" used to quantify
    /// the trip "complexity" and travel "likelihood".
    ///
    /// # Calculation
    ///
    /// Using turn-costing, we calculate immediate and summative
    /// angular rotation, and with deviance we determine a travel likelihood.
    ///
    /// ## Turn Cost
    /// We describe the summative angle, seen in the [`MapPath::total_angle`](crate::MapPath::total_angle)
    /// function, as the total angular rotation exhibited by a trip.
    /// We assume a high degree of rotation is not preferable, and trips
    /// are assumed to take the most optimal path with the most reasonable
    /// changes in trajectory, meaning many turns where few are possible
    /// is discouraged.
    ///
    /// We may then [amortize] this cost to calculate the immediately
    /// exhibited angle. Or, alternatively expressed as the average angle
    /// experienced
    ///
    /// ```math
    /// sum_angle(trip) = ∑(angles(trip))
    /// imm_angle(trip) = sum_angle(trip) / len(trip)
    ///
    /// turn_cost(trip) = imm_angle(trip)
    /// ```
    ///
    /// ## Deviance
    /// Defines the variability between the trip length (in meters)
    /// and the shortest great-circle distance between the two candidates.
    ///
    /// This cost is low in segments which follow an optimal path, i.e. in
    /// a highway, as it discourages alternate paths which may appear quicker
    /// to traverse.
    ///
    /// ```math
    /// length(trip) = ∑(distance(segment))
    /// deviance(trip, source, target) = length(trip) - distance(source, target)
    /// ```
    ///
    /// ### Total Cost
    /// The total cost is combined as such.
    ///
    /// ```math
    /// cost(trip, s, t) = deviance(trip, s, t) + turn_cost(trip)
    /// ```
    ///
    /// [amortize]: https://en.wikipedia.org/wiki/Amortized_analysis
    pub struct DefaultTransitionCost;

    impl<'a, E> Strategy<TransitionContext<'a, E>> for DefaultTransitionCost
    where
        E: Entry,
    {
        type Cost = f64;

        const ZETA: f64 = 1.;
        const BETA: f64 = 1.;

        /// Only the path's length and turning are costed.
        const WALKS_PATH: bool = false;

        #[inline]
        fn calculate(&self, context: TransitionContext<'a, E>) -> Option<Self::Cost> {
            // Find the transition lengths (shortest path, trip length)
            let lengths = context.lengths()?;

            Some(combine(
                lengths,
                context.angular_complexity(),
                context.classes(),
            ))
        }

        #[inline]
        fn summarised(&self, hop: &HopSummary) -> Option<Self::Cost> {
            Some(combine(hop.lengths?, hop.angular_complexity, hop.classes))
        }
    }

    /// The transition cost of a hop of the given lengths and turning,
    /// between edges of the given road classes.
    #[inline]
    fn combine(
        lengths: TransitionLengths,
        angular_complexity: f64,
        (source, target): (Weight, Weight),
    ) -> f64 {
        const EPSILON: f64 = 1e-6;

        // Value in range [0, 1] (1=Low Cost, 0=High Cost)
        let deviance = lengths.deviance().clamp(EPSILON, 1.0);

        // Value in range [0, 1] (1=Low Cost, 0=High Cost)
        let turn_cost = angular_complexity.clamp(EPSILON, 1.0);

        // Value in range [0, 1] (1=Low Cost, 0=High Cost)
        let class_continuity = (source as f64 / target as f64).clamp(EPSILON, 1.0);

        (deviance * turn_cost * class_continuity).sqrt()
    }
}

pub mod costing {
    use super::{DefaultEmissionCost, DefaultTransitionCost};
    use crate::costing::{
        Costing, EmissionContext, EmissionStrategy, TransitionContext, TransitionStrategy,
    };
    use core::marker::PhantomData;
    use routers_network::Entry;

    /// The costing pair a [`Matcher`](crate::Matcher) prices a match by: an
    /// emission strategy and a transition strategy, supplied together.
    ///
    /// [`CostingStrategies::default`] gives the built-in heuristics
    /// ([`DefaultEmissionCost`], [`DefaultTransitionCost`]); to bring your
    /// own, see the [`costing`](crate::costing) module documentation.
    pub struct CostingStrategies<Emmis, Trans, E>
    where
        E: Entry,
        Emmis: EmissionStrategy,
        Trans: TransitionStrategy<E>,
    {
        pub emission: Emmis,
        pub transition: Trans,

        _phantom: PhantomData<E>,
    }

    impl<Emmis, Trans, E> CostingStrategies<Emmis, Trans, E>
    where
        E: Entry,
        Emmis: EmissionStrategy,
        Trans: TransitionStrategy<E>,
    {
        pub fn new(emission: Emmis, transition: Trans) -> Self {
            Self {
                emission,
                transition,

                _phantom: PhantomData,
            }
        }
    }

    impl<E> Default for CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, E>
    where
        E: Entry,
    {
        fn default() -> Self {
            CostingStrategies::new(DefaultEmissionCost::default(), DefaultTransitionCost)
        }
    }

    impl<Emmis, Trans, E> Costing<Emmis, Trans, E> for CostingStrategies<Emmis, Trans, E>
    where
        E: Entry,
        Trans: TransitionStrategy<E>,
        Emmis: EmissionStrategy,
    {
        #[inline(always)]
        fn emission(&self, context: EmissionContext) -> u32 {
            self.emission.cost(context)
        }

        #[inline(always)]
        fn transition(&self, context: TransitionContext<E>) -> u32 {
            self.transition.cost(context)
        }
    }
}

pub use costing::*;
pub use emission::*;
pub use transition::*;
