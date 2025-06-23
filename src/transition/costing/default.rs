pub mod emission {
    use crate::transition::*;

    /// 1 meter (1/10th of the 85th% GPS error)
    const DEFAULT_EMISSION_ERROR: f64 = 1.0;

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
        /// Default: [`DEFAULT_EMISSION_ERROR`]
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

        const ZETA: f64 = 0.5;
        const BETA: f64 = -10.0; // TODO: Maybe allow dynamic parameters based on the GPS drift-?

        #[inline(always)]
        fn calculate(&self, context: EmissionContext<'a>) -> Option<Self::Cost> {
            Some(context.distance.sqrt() * context.distance)
        }
    }
}

pub mod transition {
    use crate::transition::*;
    use routers_codec::{Entry, Metadata};

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
    /// We describe the summative angle, seen in the [`Trip::total_angle`]
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

    impl<'a, E, M> Strategy<TransitionContext<'a, E, M>> for DefaultTransitionCost
    where
        E: Entry,
        M: Metadata,
    {
        type Cost = f64;

        const ZETA: f64 = 1.0;
        const BETA: f64 = -1.0;

        #[inline]
        fn calculate(&self, context: TransitionContext<'a, E, M>) -> Option<Self::Cost> {
            // Find the transition lengths (shortest path, trip length)
            let lengths = context.lengths()?;

            // Value in range [0, 1] (1=Low Cost, 0=High Cost)
            let deviance = lengths.deviance();

            // We calculate by weight, not by distinction of edges since this
            // would not uphold the invariants we intend. For example, that would
            // penalise the use of slip-roads which contain different WayIDs, despite
            // being the more-optimal path to take.
            let avg_weight = {
                let weights = context
                    .map_path
                    .windows(2)
                    .filter_map(|node| match node {
                        [a, b] if a.identifier() == b.identifier() => None,
                        [a, b] => context.routing_context.edge(a, b),
                        _ => None,
                    })
                    .map(|Edge { weight, .. }| weight as f64)
                    .collect::<Vec<_>>();

                weights.iter().sum::<f64>() / weights.len() as f64
            };

            // Value in range [0, 1] (1=Low Cost, 0=High Cost)
            let distinct_cost = (1.0 / avg_weight).powi(2).clamp(0.0, 1.0);

            // Value in range [0, 1] (1=Low Cost, 0=High Cost)
            let turn_cost = context
                .optimal_path
                .angular_complexity(context.layer_width)
                .clamp(0.0, 1.0);

            // Value in range [0, 1] (1=Low Cost, 0=High Cost)
            //  Weighted: 30% Edge Distinction, 30% Turn Difficulty, 30% Distance Deviance
            //      Note: Weights must sum to 100%
            let avg_cost = (0.3 * distinct_cost) + (0.3 * turn_cost) + (0.3 * deviance);

            // Take the inverse to "span" values
            Some(avg_cost.recip())
        }
    }
}

pub mod costing {
    use super::{DefaultEmissionCost, DefaultTransitionCost};
    use crate::transition::*;
    use routers_codec::{Entry, Metadata};
    use std::marker::PhantomData;

    pub struct CostingStrategies<Emmis, Trans, E, M>
    where
        E: Entry,
        M: Metadata,
        Emmis: EmissionStrategy,
        Trans: TransitionStrategy<E, M>,
    {
        emission: Emmis,
        transition: Trans,

        _phantom: std::marker::PhantomData<E>,
        _phantom2: std::marker::PhantomData<M>,
    }

    impl<Emmis, Trans, E, M> CostingStrategies<Emmis, Trans, E, M>
    where
        E: Entry,
        M: Metadata,
        Emmis: EmissionStrategy,
        Trans: TransitionStrategy<E, M>,
    {
        pub fn new(emission: Emmis, transition: Trans) -> Self {
            Self {
                emission,
                transition,

                _phantom: PhantomData,
                _phantom2: PhantomData,
            }
        }
    }

    impl<E, M> Default for CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, E, M>
    where
        E: Entry,
        M: Metadata,
    {
        fn default() -> Self {
            CostingStrategies::new(DefaultEmissionCost::default(), DefaultTransitionCost)
        }
    }

    impl<Emmis, Trans, E, M> Costing<Emmis, Trans, E, M> for CostingStrategies<Emmis, Trans, E, M>
    where
        E: Entry,
        M: Metadata,
        Trans: TransitionStrategy<E, M>,
        Emmis: EmissionStrategy,
    {
        #[inline(always)]
        fn emission(&self, context: EmissionContext) -> u32 {
            self.emission.cost(context)
        }

        #[inline(always)]
        fn transition(&self, context: TransitionContext<E, M>) -> u32 {
            self.transition.cost(context)
        }
    }
}

pub use costing::*;
pub use emission::*;
pub use transition::*;
