use buffa::view::OwnedView;
use connectrpc::{ConnectError, RequestContext, ServiceResult};
use geo::Point;
use routers_network::{Entry, Metadata, Network};
use schema::connect::routers::api::optimise::v1::OptimiseService;
use schema::proto::routers::api::optimise::v1::{__buffa::view::RouteRequestView, RouteResponse};
#[cfg(feature = "telemetry")]
use tracing::Level;

use crate::sdk::r#match::coordinate;
use crate::services::RPCAdapter;

#[allow(refining_impl_trait)]
impl<T, E, M> OptimiseService for RPCAdapter<T, E, M>
where
    T: Network<E, M> + Send + Sync + 'static,
    M: Metadata + Send + Sync + 'static,
    E: Entry + Send + Sync + 'static,
{
    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn route(
        &self,
        _ctx: RequestContext,
        request: OwnedView<RouteRequestView<'static>>,
    ) -> ServiceResult<RouteResponse> {
        let owned = request.to_owned_message();

        let start = owned
            .start
            .as_option()
            .map(|c| Point::new(c.longitude, c.latitude))
            .ok_or_else(|| ConnectError::invalid_argument("Missing Start Coordinate"))?;

        let end = owned
            .end
            .as_option()
            .map(|c| Point::new(c.longitude, c.latitude))
            .ok_or_else(|| ConnectError::invalid_argument("Missing End Coordinate"))?;

        let (cost, route) = self
            .inner
            .route_points(&start, &end)
            .ok_or_else(|| ConnectError::internal("Could not route"))?;

        let shape = route
            .iter()
            .map(|node| coordinate(node.position.into()))
            .collect();

        Ok(RouteResponse {
            cost,
            shape,
            ..Default::default()
        }
        .into())
    }
}
