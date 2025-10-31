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

        #[inline(always)]
        fn calculate(&self, context: EmissionContext<'a>) -> Option<Self::Cost> {
            Some(DEFAULT_EMISSION_ERROR / context.distance.powi(5))
        }
    }
}

pub mod transition {
    use crate::Graph;
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

        #[inline]
        fn calculate(&self, context: TransitionContext<'a, E, M>) -> Option<Self::Cost> {
            // Values in range [0, 1] (1=Low Cost, 0=High Cost)
            let distinct_cost =
                Self::travel_cost::<E, M>(context.map_path, context.routing_context.map);
            let turn_cost = Self::turn_cost::<E>(&context.optimal_path);
            let deviance_cost = Self::deviance_cost::<E, M>(&context)?;

            // Value in range [0, 1] (1=Low Cost, 0=High Cost)
            //  Weighted: 60% Turn Difficulty, 30% Edge Distinction, 10% Distance Deviance
            //      Note: Weights must sum to 100%
            let avg_cost = (0.6 * turn_cost) + (0.3 * distinct_cost) + (0.1 * deviance_cost);
            Some(avg_cost)
        }
    }

    impl DefaultTransitionCost {
        pub(super) fn deviance_cost<E: Entry, M: Metadata>(
            context: &TransitionContext<E, M>,
        ) -> Option<f64> {
            context.lengths().map(|v| v.deviance())
        }

        pub(super) fn turn_cost<E: Entry>(optimal_path: &Trip<E>) -> f64 {
            optimal_path.angular_complexity().clamp(0.0, 1.0)
        }

        pub(crate) fn travel_cost<E: Entry, M: Metadata>(path: &[E], map: &Graph<E, M>) -> f64 {
            // We calculate by weight, not by distinction of edges since this
            // would not uphold the invariants we intend. For example, that would
            // penalise the use of slip-roads which contain different WayIDs, despite
            // being the more-optimal path to take.
            let avg_weight = {
                let weights = path
                    .windows(2)
                    .filter_map(|node| match node {
                        [a, b] => map.edge(a, b),
                        _ => None,
                    })
                    .map(|Edge { weight, .. }| {
                        return weight as f64;
                    })
                    .collect::<Vec<_>>();

                weights.iter().sum::<f64>() / weights.len() as f64
            };

            // Value in range [0, 1] (1=Low Cost, 0=High Cost)
            (1.0 / avg_weight).powi(2).clamp(0.0, 1.0)
        }
    }

    #[cfg(test)]
    mod test {
        use super::*;
        use approx::assert_relative_eq;
        use geo::{Distance, Haversine};
        use routers_codec::osm::OsmEntryId;
        use routers_fixtures::{LOS_ANGELES, fixture};

        const REMAIN_ON_HIGHWAY: [OsmEntryId; 18] = [
            OsmEntryId::node(1233732718),
            OsmEntryId::node(359024313),
            OsmEntryId::node(529607857),
            OsmEntryId::node(7662616751),
            OsmEntryId::node(7663077001),
            OsmEntryId::node(359024315),
            OsmEntryId::node(7663077030),
            OsmEntryId::node(7662616710),
            OsmEntryId::node(7663077017),
            OsmEntryId::node(529607860),
            OsmEntryId::node(7663077004),
            OsmEntryId::node(529607863),
            OsmEntryId::node(7663077033),
            OsmEntryId::node(7662616705),
            OsmEntryId::node(7663077020),
            OsmEntryId::node(7662616741),
            OsmEntryId::node(7663077007),
            OsmEntryId::node(529607866),
        ];

        const TAKE_OFFRAMP: [OsmEntryId; 27] = [
            OsmEntryId::node(1233732718),
            OsmEntryId::node(6543305229),
            OsmEntryId::node(7664051912),
            OsmEntryId::node(1233732754),
            OsmEntryId::node(1233732753),
            OsmEntryId::node(1233732752),
            OsmEntryId::node(1233732749),
            OsmEntryId::node(1233732746),
            OsmEntryId::node(11502558958),
            OsmEntryId::node(1233732744),
            OsmEntryId::node(1233732739),
            OsmEntryId::node(12191918571),
            OsmEntryId::node(12191918570),
            OsmEntryId::node(19668244),
            OsmEntryId::node(529607908),
            OsmEntryId::node(529607910),
            OsmEntryId::node(529607911),
            OsmEntryId::node(529607912),
            OsmEntryId::node(529607913),
            OsmEntryId::node(529607914),
            OsmEntryId::node(529607915),
            OsmEntryId::node(7663981871),
            OsmEntryId::node(529607916),
            OsmEntryId::node(529607917),
            OsmEntryId::node(6543305223),
            OsmEntryId::node(529607919),
            OsmEntryId::node(529607866),
        ];

        #[test]
        fn assert_highway_better_than_offramp_travel_cost() {
            let path = std::path::Path::new(fixture!(LOS_ANGELES))
                .as_os_str()
                .to_ascii_lowercase();

            let map = Graph::new(path).expect("must initialise");

            let remain_cost = DefaultTransitionCost::travel_cost(&REMAIN_ON_HIGHWAY, &map);
            let off_cost = DefaultTransitionCost::travel_cost(&TAKE_OFFRAMP, &map);

            // 1=No Cost, 0=Inf Cost : We want it to be "cheaper" (higher) to remain, than get off.
            assert!(remain_cost > off_cost);

            assert_relative_eq!(remain_cost, 1.0);
            assert_relative_eq!(off_cost, 0.25);
        }

        #[test]
        fn assert_highway_better_than_offramp_turn_cost() {
            let path = std::path::Path::new(fixture!(LOS_ANGELES))
                .as_os_str()
                .to_ascii_lowercase();

            let map = Graph::new(path).expect("must initialise");

            let remain = Trip::new_with_map(&map, &REMAIN_ON_HIGHWAY);
            let off = Trip::new_with_map(&map, &TAKE_OFFRAMP);

            let remain_cost = DefaultTransitionCost::turn_cost(&remain);
            let off_cost = DefaultTransitionCost::turn_cost(&off);

            // 1=No Cost, 0=Inf Cost : We want it to be "cheaper" (higher) to remain, than get off.
            assert!(remain_cost > off_cost);
        }

        #[test]
        fn assert_highway_better_than_offramp_deviance() {
            let path = std::path::Path::new(fixture!(LOS_ANGELES))
                .as_os_str()
                .to_ascii_lowercase();

            let map = Graph::new(path).expect("must initialise");

            let lengths = |values: &[OsmEntryId]| {
                let start = map
                    .get_position(values.first().expect("must contain a start point"))
                    .expect("must resolve to a location");

                let end = map
                    .get_position(values.last().expect("must contain an end point"))
                    .expect("must resolve to a location");

                let straightline_distance = Haversine.distance(start, end);
                let route = Trip::new_with_map(&map, values);

                TransitionLengths {
                    straightline_distance,
                    route_length: route.length(),
                }
            };

            let remain = lengths(&REMAIN_ON_HIGHWAY);
            let offramp = lengths(&TAKE_OFFRAMP);

            eprintln!("Remain={:?}", remain);
            eprintln!("Offramp={:?}", offramp);

            let remain_deviance = remain.deviance();
            let offramp_deviance = offramp.deviance();

            // We want the remain-on-highway to have a lower deviance than the offramp.
            assert!(remain_deviance > offramp_deviance);

            assert_relative_eq!(remain_deviance, 0.998, epsilon = 1e-2f64);
            assert_relative_eq!(offramp_deviance, 0.965, epsilon = 1e-2f64);
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
        pub emission: Emmis,
        pub transition: Trans,

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
        fn emission(&self, context: EmissionContext) -> f64 {
            self.emission.cost(context)
        }

        #[inline(always)]
        fn transition(&self, context: TransitionContext<E, M>) -> f64 {
            self.transition.cost(context)
        }
    }
}

pub use costing::*;
pub use emission::*;
pub use transition::*;
