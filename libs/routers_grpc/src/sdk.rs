//! Defines internal translations and relevant utilities
//! in order to make the model useful as an SDK.

use geo::{Coord};
use routers_network::{Entry, Metadata, Node};

pub mod optimise {
    use buffa::EnumValue;
    use routers::SolverVariant;
    use schema::proto::routers::model::v1::OptimiseFor;

    pub fn optimise_for(value: EnumValue<OptimiseFor>) -> SolverVariant {
        match value {
            OptimiseFor::Unspecified | OptimiseFor::Speed => SolverVariant::Fastest,
            OptimiseFor::Consistency => SolverVariant::Selective,
            OptimiseFor::Parallelism => SolverVariant::Precompute,
        }
    }
}

pub mod r#match {
    use buffa::RepeatedView;
    use geo::{Coord, LineString};
    use routers_codec::primitive::{context::TripContext, transport::{TransportMode, TruckCosting, VehicleCosting}};
    use routers_network::Metadata;

    use schema::proto::routers::model::v1::{CoordinateView, CostOptions, Costing, costing::{BusModel, CarModel, TruckModel, Variation}};

    pub fn truck_costing(model: TruckModel) -> TruckCosting {
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

    pub fn car_costing(model: CarModel) -> VehicleCosting {
        VehicleCosting {
            height: model.height,
            width: model.width,
        }
    }

    pub fn bus_costing(model: BusModel) -> VehicleCosting {
        VehicleCosting {
            height: model.height,
            width: model.width,
        }
    }

    pub fn as_linestring<'a>(value: RepeatedView<'a, CoordinateView<'a>>) -> LineString {
        value.iter()
            .map(|v| Coord { x: v.latitude, y: v.longitude })
            .collect::<LineString>()
    }

    pub fn trip_context<'a, M: Metadata>(costing: &'a Costing) -> M::TripContext
    where
        M::TripContext: From<CostOptions>,
    {
        let transport_mode = match costing.variation? {
            Variation::Bus(bus) =>
                TransportMode::Bus(Some(bus_costing(*bus))),
            Variation::Car(car) =>
                TransportMode::Car(Some(car_costing(*car))),
            Variation::Truck(truck) =>
                TransportMode::Truck(Some(truck_costing(*truck))),
        };

        TripContext { transport_mode }
    }

    pub fn interpolated(out: ) -> Option<LineString> {
        let path = self
            .matches
            .first()?
            .interpolated
            .iter()
            .filter_map(|element| element.coordinate)
            .collect::<Vec<_>>();

        Some(Coordinates(path))
    }

    pub fn discretized(&self) -> Option<LineString> {
        let path = self
            .matches
            .first()?
            .discretized
            .iter()
            .filter_map(|element| element.coordinate)
            .collect::<Vec<_>>();

        Some(Coordinates(path))
    }
}
