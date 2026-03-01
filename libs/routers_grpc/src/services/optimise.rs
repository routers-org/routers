use alloc::sync::Arc;
use geo::{Point, coord};
use routers_network::Network;
use tonic::{Request, Response, Status};

use crate::definition::model::*;
use crate::definition::optimise::*;

use crate::services::GrpcAdapter;
use routers_network::{Entry, Metadata};

#[cfg(feature = "telemetry")]
use tracing::Level;

#[tonic::async_trait]
impl<T, E, M> OptimiseService for GrpcAdapter<T, E, M>
where
    T: Network<E, M> + Send + Sync + 'static,
    M: Metadata + Send + Sync + 'static,
    E: Entry + Send + Sync + 'static,
{
    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, err(level = Level::INFO)))]
    async fn route(
        self: Arc<Self>,
        request: Request<RouteRequest>,
    ) -> Result<Response<RouteResponse>, Status> {
        let (_, _, routing) = request.into_parts();

        let start = routing
            .start
            .map_or(
                Err(Status::invalid_argument("Missing Start Coordinate")),
                |v| Ok(coord! { x: v.longitude, y: v.latitude }),
            )
            .map_err(|err| Status::internal(format!("{err:?}")))?;

        let end = routing
            .end
            .map_or(
                Err(Status::invalid_argument("Missing End Coordinate")),
                |v| Ok(coord! { x: v.longitude, y: v.latitude }),
            )
            .map_err(|err| Status::internal(format!("{err:?}")))?;

        self.inner.route_points(&Point(start), &Point(end)).map_or(
            Err(Status::internal("Could not route")),
            |(cost, route)| {
                let shape = route
                    .iter()
                    .map(|node| Coordinate {
                        latitude: node.position.y(),
                        longitude: node.position.x(),
                    })
                    .collect();

                Ok(Response::new(RouteResponse { cost, shape }))
            },
        )
    }
}
