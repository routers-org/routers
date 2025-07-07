//! Defines internal translations and relevant utilities
//! in order to make the model useful as an SDK.

use crate::r#match::{MatchRequest, MatchResponse, SnapRequest};
use crate::model::costing::{BusModel, CarModel, TruckModel, Variation};
use crate::model::{
    Coordinate, CostOptions, EdgeIdentifier, EdgeMetadata, NodeIdentifier, OptimiseFor,
};

use geo::{Coord, LineString, coord};
use routers::SolverVariant;
use routers_codec::osm::OsmTripConfiguration;
use routers_codec::osm::meta::OsmEdgeMetadata;
use routers_codec::osm::speed_limit::{SpeedLimitConditions, SpeedLimitExt};
use routers_codec::primitive::context::TripContext;
use routers_codec::primitive::transport::{TransportMode, TruckCosting, VehicleCosting};
use routers_codec::{Entry, Node};
use std::fmt::Error as StdError;
use std::ops::Deref;

impl From<Coord> for Coordinate {
    fn from(value: Coord) -> Self {
        Coordinate {
            longitude: value.x,
            latitude: value.y,
        }
    }
}

pub struct Coordinates(Vec<Coordinate>);

impl From<Coordinates> for LineString {
    fn from(val: Coordinates) -> Self {
        val.iter()
            .map(|c| coord! { x: c.longitude, y: c.latitude })
            .collect::<LineString>()
    }
}

impl From<LineString> for Coordinates {
    fn from(value: LineString) -> Self {
        Coordinates(value.into_iter().map(|point| point.into()).collect())
    }
}

impl Coordinates {
    pub fn linestring(self) -> LineString {
        LineString::from(self)
    }
}

impl Deref for Coordinates {
    type Target = Vec<Coordinate>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl MatchResponse {
    pub fn interpolated(&self) -> Option<Coordinates> {
        let path = self
            .matches
            .first()?
            .interpolated
            .iter()
            .filter_map(|element| element.coordinate)
            .collect::<Vec<_>>();

        Some(Coordinates(path))
    }

    pub fn discretized(&self) -> Option<Coordinates> {
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

type MetadataAndTraversal<'a> = (&'a OsmEdgeMetadata, &'a OsmTripConfiguration);

impl From<MetadataAndTraversal<'_>> for EdgeMetadata {
    fn from((meta, cond): MetadataAndTraversal<'_>) -> Self {
        // TODO: Fill all the information out here...
        EdgeMetadata {
            lane_count: meta.lane_count.map(|v| v.get() as u32),
            speed_limit: meta
                .speed_limit
                .as_ref()
                .map(|v| v.relevant_limits(cond, SpeedLimitConditions::default()))
                .and_then(|v| v.first().map(|elem| elem.speed))
                .and_then(|v| v.in_kmh())
                .map(|speed| speed.get() as u32),
            names: vec![],
        }
    }
}

impl From<i64> for EdgeIdentifier {
    fn from(val: i64) -> Self {
        EdgeIdentifier { id: val }
    }
}

impl<E> From<Node<E>> for NodeIdentifier
where
    E: Entry,
{
    fn from(Node { id, position }: Node<E>) -> Self {
        NodeIdentifier {
            id: id.identifier(),
            coordinate: Some(<geo::Point as Into<Coord>>::into(position).into()),
        }
    }
}

impl MatchRequest {
    pub fn linestring(&self) -> LineString {
        Into::<LineString>::into(Coordinates(self.data.clone()))
    }
}

impl SnapRequest {
    pub fn linestring(&self) -> LineString {
        Into::<LineString>::into(Coordinates(self.data.clone()))
    }
}

impl TryFrom<MatchResponse> for LineString {
    type Error = StdError;

    fn try_from(value: MatchResponse) -> Result<Self, Self::Error> {
        let linestring = value.discretized().ok_or(StdError)?.into();

        Ok(linestring)
    }
}

impl From<TruckModel> for TruckCosting {
    fn from(model: TruckModel) -> Self {
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
}

impl From<CarModel> for VehicleCosting {
    fn from(model: CarModel) -> Self {
        VehicleCosting {
            height: model.height,
            width: model.width,
        }
    }
}

impl From<BusModel> for VehicleCosting {
    fn from(model: BusModel) -> Self {
        VehicleCosting {
            height: model.height,
            width: model.width,
        }
    }
}

impl From<CostOptions> for Option<TripContext> {
    fn from(value: CostOptions) -> Option<TripContext> {
        let transport_mode = match value.costing_method?.variation? {
            Variation::Bus(bus) => TransportMode::Bus(Some(VehicleCosting::from(bus))),
            Variation::Car(car) => TransportMode::Car(Some(VehicleCosting::from(car))),
            Variation::Truck(truck) => TransportMode::Truck(Some(TruckCosting::from(truck))),
        };

        Some(TripContext { transport_mode })
    }
}

impl From<Option<CostOptions>> for OptimiseFor {
    fn from(value: Option<CostOptions>) -> Self {
        OptimiseFor::try_from(value.unwrap_or_default().optimise_for).unwrap_or_default()
    }
}
