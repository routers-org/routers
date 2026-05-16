use crate::sdk::r#match::coordinate;
use crate::services::RPCAdapter;
use buffa::MessageField;
use buffa::view::OwnedView;
use connectrpc::{ConnectError, RequestContext, ServiceResult};
use core::cmp::Ordering;
use geo::{Distance, Haversine, Point};
use log::{debug, info};
use routers_network::{Entry, Metadata, Network};
use schema::connect::routers::api::scan::v1::ScanService;
use schema::proto::routers::api::scan::v1::{
    __buffa::view::{EdgeRequestView, PointRequestView, PointSnappedRequestView},
    EdgeResponse, PointResponse, PointSnappedResponse,
};
#[cfg(feature = "telemetry")]
use tracing::Level;

#[allow(refining_impl_trait)]
impl<T, E, M> ScanService for RPCAdapter<T, E, M>
where
    T: Network<E, M> + Send + Sync + 'static,
    M: Metadata + Send + Sync + 'static,
    E: Entry + Send + Sync + 'static,
{
    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn point(
        &self,
        _ctx: RequestContext,
        request: OwnedView<PointRequestView<'static>>,
    ) -> ServiceResult<PointResponse> {
        let owned = request.to_owned_message();

        let point = owned
            .coordinate
            .as_option()
            .map(|c| Point::new(c.longitude, c.latitude))
            .ok_or_else(|| ConnectError::invalid_argument("Missing Coordinate"))?;

        let nearest = self
            .inner
            .nearest_node(&point)
            .ok_or_else(|| ConnectError::internal("Could not find appropriate point"))?;

        Ok(PointResponse {
            coordinate: MessageField::some(coordinate(nearest.position.into())),
            ..Default::default()
        }
        .into())
    }

    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn point_snapped(
        &self,
        _ctx: RequestContext,
        request: OwnedView<PointSnappedRequestView<'static>>,
    ) -> ServiceResult<PointSnappedResponse> {
        let owned = request.to_owned_message();

        let point = owned
            .coordinate
            .as_option()
            .map(|c| Point::new(c.longitude, c.latitude))
            .ok_or_else(|| ConnectError::invalid_argument("Missing Point"))?;

        info!(
            "Got request for ({}, {}) within {} square meters",
            point.x(),
            point.y(),
            owned.search_radius
        );

        let mut nearest_points = self
            .inner
            .nearest_nodes_projected(&point, owned.search_radius)
            .collect::<Vec<_>>();

        debug!("Found {} points", nearest_points.len());

        nearest_points.sort_by(|(a, _), (b, _)| {
            let dist_a = Haversine.distance(point, *a);
            let dist_b = Haversine.distance(point, *b);
            dist_a.partial_cmp(&dist_b).unwrap_or(Ordering::Equal)
        });

        let nearest = nearest_points
            .first()
            .ok_or_else(|| ConnectError::internal("Could not find appropriate point"))?;

        Ok(PointSnappedResponse {
            coordinate: MessageField::some(coordinate(nearest.0.0)),
            ..Default::default()
        }
        .into())
    }

    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn edge(
        &self,
        _ctx: RequestContext,
        _request: OwnedView<EdgeRequestView<'static>>,
    ) -> ServiceResult<EdgeResponse> {
        Err(ConnectError::unimplemented("edge lookup not implemented"))
    }
}
