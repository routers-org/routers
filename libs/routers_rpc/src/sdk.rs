//! Defines internal translations and relevant utilities
//! in order to make the model useful as an SDK.

pub mod optimise {
    use buffa::EnumValue;
    use routers::weigh::SolverVariant;
    use schema::proto::routers::model::v1::OptimiseFor;

    pub fn optimise_for(value: EnumValue<OptimiseFor>) -> SolverVariant {
        match value.as_known().unwrap_or_default() {
            OptimiseFor::OPTIMISE_FOR_UNSPECIFIED | OptimiseFor::OPTIMISE_FOR_SPEED => {
                SolverVariant::Fastest
            }
            OptimiseFor::OPTIMISE_FOR_CONSISTENCY => SolverVariant::Selective,
            OptimiseFor::OPTIMISE_FOR_PARALLELISM => SolverVariant::Precompute,
        }
    }
}

pub mod r#match {
    use buffa::RepeatedView;
    use geo::{Coord, LineString};
    use routers_codec::osm::speed_limit::{SpeedLimitConditions, SpeedLimitExt};
    use routers_codec::osm::{OsmEdgeMetadata, OsmTripConfiguration};
    use routers_codec::primitive::context::TripContext;
    use routers_codec::primitive::transport::{TransportMode, TruckCosting, VehicleCosting};
    use routers_network::Metadata;

    use schema::proto::routers::model::v1::costing::{BusModel, CarModel, TruckModel, Variation};
    use schema::proto::routers::model::v1::{Coordinate, CoordinateView, Costing, EdgeMetadata};

    pub fn truck_costing(model: &TruckModel) -> TruckCosting {
        TruckCosting {
            vehicle_costing: VehicleCosting {
                height: model.height,
                width: model.width,
            },
            length: model.length,
            axle_load: model.axle_load,
            axle_count: model.axle_count as u8,
            hazmat_load: model.hazardous_load,
        }
    }

    pub fn car_costing(model: &CarModel) -> VehicleCosting {
        VehicleCosting {
            height: model.height,
            width: model.width,
        }
    }

    pub fn bus_costing(model: &BusModel) -> VehicleCosting {
        VehicleCosting {
            height: model.height,
            width: model.width,
        }
    }

    pub fn coordinate(value: Coord) -> Coordinate {
        Coordinate {
            longitude: value.x,
            latitude: value.y,
            ..Default::default()
        }
    }

    pub fn as_linestring(value: &RepeatedView<'_, CoordinateView<'_>>) -> LineString {
        value
            .iter()
            .map(|v| Coord {
                x: v.longitude,
                y: v.latitude,
            })
            .collect::<LineString>()
    }

    /// Convert a [`Costing`] message into an OSM-domain [`TripContext`].
    pub fn osm_trip_context(costing: &Costing) -> Option<TripContext> {
        let transport_mode = match costing.variation.as_ref()? {
            Variation::Bus(bus) => TransportMode::Bus(Some(bus_costing(bus))),
            Variation::Car(car) => TransportMode::Car(Some(car_costing(car))),
            Variation::Truck(truck) => TransportMode::Truck(Some(truck_costing(truck))),
        };

        Some(TripContext { transport_mode })
    }

    /// Build an [`EdgeMetadata`] view from an OSM edge's intrinsic metadata
    /// and the trip's runtime configuration.
    pub fn osm_edge_metadata(
        meta: &OsmEdgeMetadata,
        runtime: &OsmTripConfiguration,
    ) -> EdgeMetadata {
        let speed_limit = meta
            .speed_limit
            .as_ref()
            .map(|v| v.relevant_limits(runtime, SpeedLimitConditions::default()))
            .and_then(|v| v.first().map(|elem| elem.speed.clone()))
            .and_then(|v| v.in_kmh())
            .map(|speed| speed.get() as u32);

        EdgeMetadata {
            lane_count: meta.lane_count.map(|v| v.get() as u32),
            speed_limit,
            names: ::buffa::alloc::vec::Vec::new(),
            ..Default::default()
        }
    }

    /// Generic glue for [`MatchService`] handlers: lifts the OSM-specific
    /// conversions into a trait so the handler can stay generic over the
    /// network's [`Metadata`] implementation. Implement for new metadata
    /// types alongside their [`Metadata`] impl.
    pub trait MatchSdk: Metadata {
        fn trip_context(costing: &Costing) -> Option<Self::TripContext>;
        fn edge_metadata(meta: &Self, runtime: &Self::Runtime) -> EdgeMetadata;
    }

    impl MatchSdk for OsmEdgeMetadata {
        fn trip_context(costing: &Costing) -> Option<TripContext> {
            osm_trip_context(costing)
        }

        fn edge_metadata(meta: &Self, runtime: &OsmTripConfiguration) -> EdgeMetadata {
            osm_edge_metadata(meta, runtime)
        }
    }
}
