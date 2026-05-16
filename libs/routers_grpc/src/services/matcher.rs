use alloc::sync::Arc;
use buffa::MessageField;
use buffa::OwnedView;
use connectrpc::ConnectError;
use connectrpc::RequestContext;
use connectrpc::ServiceResult;
use core::marker::PhantomData;
use geo::{Distance, Geodesic};
use routers::r#match::MatchOptions;
use routers_network::Network;
use schema::proto::routers::api::r#match::v1::MatchRequestView;
use schema::proto::routers::api::r#match::v1::MatchResponse;
use schema::proto::routers::api::r#match::v1::SnapRequest;
use schema::proto::routers::api::r#match::v1::SnapResponse;
use schema::proto::routers::model::v1::OptimiseFor;
use std::ops::Deref;
use tonic::ConnectError;
use tonic::{Request, Response, Status};

use schema::connect::routers::api::r#match::v1::*;
use schema::proto::routers::model::v1::*;

use routers::{Match, Path, RoutedPath};
use routers_network::{Entry, Metadata};
#[cfg(feature = "telemetry")]
use tracing::Level;

use crate::sdk::r#match::as_linestring;
use crate::sdk::r#match::trip_context;
use crate::sdk::optimise::optimise_for;

struct Util<Ctx>(PhantomData<Ctx>);

impl<Ctx> Util<Ctx> {
    fn route_from_path<E: Entry, M: Metadata>(input: Path<E, M>, ctx: &Ctx) -> Vec<RouteElement>
    where
        EdgeMetadata: for<'a> From<(&'a M, &'a Ctx)>,
    {
        input
            .iter()
            .flat_map(|entry| {
                let edge_id = EdgeIdentifier {
                    id: entry.edge.id().identifier(),
                    ..Default::default()
                };

                let source_id = NodeIdentifier {
                    id: entry.edge.source.identifier(),
                    ..Default::default()
                };

                let target_id = NodeIdentifier {
                    id: entry.edge.target.identifier(),
                    ..Default::default()
                };

                let edge = Edge {
                    id: MessageField::some(edge_id),
                    source: MessageField::some(source_id),
                    target: MessageField::some(target_id),
                    length: Geodesic
                        .distance(entry.edge.source.position, entry.edge.target.position),
                    metadata: EdgeMetadata::from((&entry.metadata, ctx)),
                    ..Default::default()
                };

                let edge = Edge::default()
                    .id(entry.edge.id().identifier())
                    .source(entry.edge.source)
                    .target(entry.edge.target)
                    .metadata(EdgeMetadata::from((&entry.metadata, ctx)))
                    .length(
                        Geodesic.distance(entry.edge.source.position, entry.edge.target.position),
                    )
                    .build()
                    .unwrap();

                RouteElement {
                    coordinate: MessageField::some(Coordinate::from(entry.point)),
                    edge: MessageField::some(RouteEdge {
                        edge: MessageField::some(edge),
                        ..RouteEdge::default()
                    }),
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>()
    }

    fn process<E: Entry, M: Metadata>(result: RoutedPath<E, M>, ctx: Ctx) -> Vec<MatchedRoute>
    where
        EdgeMetadata: for<'a> From<(&'a M, &'a Ctx)>,
    {
        let interpolated = Util::route_from_path(result.interpolated, &ctx);
        let discretized = Util::route_from_path(result.discretized, &ctx);

        let matched_route = MatchedRoute {
            interpolated,
            discretized,
            cost: 0,
            ..Default::default()
        };

        vec![matched_route]
    }
}

struct MatchState<T, E, M> {
    inner: Arc<T>,
    _marker: PhantomData<(E, M)>,
}

impl<T, E, M> MatchService for MatchState<T, E, M>
where
    T: Network<E, M> + Send + Sync + 'static,
    M: Metadata + Send + Sync + 'static,
    E: Entry + Send + Sync + 'static,
    EdgeMetadata: for<'a> From<(&'a M, &'a M::Runtime)>,
    Option<M::TripContext>: From<CostOptions>,
{
    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn r#match(
        &self,
        ctx: RequestContext,
        request: OwnedView<MatchRequestView<'static>>,
    ) -> ServiceResult<MatchResponse> {
        let coordinates = as_linestring(request.data);
        let context = request
            .to_owned_message()
            .options
            .costing_method
            .as_option()
            .map(|view| trip_context(view));

        // Find which solver to use...
        let solver = optimise_for(request.options.optimise_for);
        let runtime = M::runtime(context);

        let opts = MatchOptions::new()
            .with_runtime(runtime.clone())
            .with_solver(solver)
            .with_search_distance(request.search_distance);

        let result = self
            .inner
            .r#match(coordinates, opts)
            .map_err(|e| e.to_string())
            .map_err(ConnectError::internal)?;

        Ok(MatchResponse {
            matches: Util::<M::Runtime>::process(result, runtime),
            ..Default::default()
        })
    }

    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn snap(
        self: Arc<Self>,
        request: Request<SnapRequest>,
    ) -> Result<Response<SnapResponse>, Status> {
        let (.., message) = request.into_parts();
        let coordinates = message.linestring();

        // Find which solver to use...
        let solver = OptimiseFor::from(message.options);
        let runtime = M::runtime(message.trip_context::<M>());

        let opts = MatchOptions::new()
            .with_runtime(runtime.clone())
            .with_solver(solver)
            .with_search_distance(message.search_distance);

        let result = self
            .inner
            .snap(coordinates, opts)
            .map_err(|e| e.to_string())
            .map_err(Status::internal)?;

        Ok(Response::new(SnapResponse {
            matches: Util::<M::Runtime>::process(result, runtime),
        }))
    }
}
