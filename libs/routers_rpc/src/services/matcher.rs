use buffa::MessageField;
use buffa::view::OwnedView;
use connectrpc::{ConnectError, RequestContext, ServiceResult};
use core::marker::PhantomData;
use geo::{Distance, Geodesic};
use routers_network::Network;
use schema::connect::routers::api::r#match::v1::MatchService;
use schema::proto::routers::api::r#match::v1::{
    __buffa::view::{MatchRequestView, SnapRequestView},
    MatchResponse, SnapResponse,
};
use schema::proto::routers::model::v1::{
    Edge, EdgeIdentifier, MatchedRoute, NodeIdentifier, RouteEdge, RouteElement,
};

use routers_network::{Entry, Metadata};
use routers_transition::{Match, Path, RoutedPath, r#match::MatchOptions};
#[cfg(feature = "telemetry")]
use tracing::Level;

use crate::sdk::r#match::{MatchSdk, as_linestring, coordinate};
use crate::sdk::optimise::optimise_for;
use crate::services::RPCAdapter;

struct Util<Ctx>(PhantomData<Ctx>);

impl<Ctx> Util<Ctx> {
    fn route_from_path<E, M>(input: Path<E, M>, ctx: &Ctx) -> Vec<RouteElement>
    where
        E: Entry,
        M: MatchSdk<Runtime = Ctx>,
    {
        input
            .iter()
            .map(|entry| {
                let edge = Edge {
                    id: MessageField::some(EdgeIdentifier {
                        id: entry.edge.id().identifier(),
                        ..Default::default()
                    }),
                    source: MessageField::some(NodeIdentifier {
                        id: entry.edge.source.identifier(),
                        coordinate: MessageField::some(coordinate(
                            entry.edge.source.position.into(),
                        )),
                        ..Default::default()
                    }),
                    target: MessageField::some(NodeIdentifier {
                        id: entry.edge.target.identifier(),
                        coordinate: MessageField::some(coordinate(
                            entry.edge.target.position.into(),
                        )),
                        ..Default::default()
                    }),
                    length: Geodesic
                        .distance(entry.edge.source.position, entry.edge.target.position),
                    metadata: MessageField::some(M::edge_metadata(&entry.metadata, ctx)),
                    ..Default::default()
                };

                RouteElement {
                    coordinate: MessageField::some(coordinate(entry.point.into())),
                    edge: MessageField::some(RouteEdge {
                        edge: MessageField::some(edge),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            })
            .collect::<Vec<_>>()
    }

    fn process<E, M>(result: RoutedPath<E, M>, ctx: M::Runtime) -> Vec<MatchedRoute>
    where
        E: Entry,
        M: MatchSdk<Runtime = Ctx>,
    {
        let interpolated = Util::<Ctx>::route_from_path::<E, M>(result.interpolated, &ctx);
        let discretized = Util::<Ctx>::route_from_path::<E, M>(result.discretized, &ctx);

        vec![MatchedRoute {
            interpolated,
            discretized,
            cost: 0,
            ..Default::default()
        }]
    }
}

#[allow(refining_impl_trait)]
impl<T, E, M> MatchService for RPCAdapter<T, E, M>
where
    T: Network<E, M> + Send + Sync + 'static,
    M: Metadata + MatchSdk + Send + Sync + 'static,
    E: Entry + Send + Sync + 'static,
{
    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn r#match(
        &self,
        _ctx: RequestContext,
        request: OwnedView<MatchRequestView<'static>>,
    ) -> ServiceResult<MatchResponse> {
        let owned = request.to_owned_message();

        let coordinates = as_linestring(&request.data);
        let context = owned
            .options
            .as_option()
            .and_then(|opts| opts.costing_method.as_option())
            .and_then(M::trip_context);

        let solver = optimise_for(
            owned
                .options
                .as_option()
                .map(|o| o.optimise_for)
                .unwrap_or_default(),
        );
        let runtime = M::runtime(context);

        let opts = MatchOptions::new()
            .with_runtime(runtime.clone())
            .with_solver(solver)
            .with_search_distance(owned.search_distance);

        let result = self
            .inner
            .r#match(coordinates, opts)
            .map_err(|e| e.to_string())
            .map_err(ConnectError::internal)?;

        Ok(MatchResponse {
            matches: Util::<M::Runtime>::process::<E, M>(result, runtime),
            ..Default::default()
        }
        .into())
    }

    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn snap(
        &self,
        _ctx: RequestContext,
        request: OwnedView<SnapRequestView<'static>>,
    ) -> ServiceResult<SnapResponse> {
        let owned = request.to_owned_message();

        let coordinates = as_linestring(&request.data);
        let context = owned
            .options
            .as_option()
            .and_then(|opts| opts.costing_method.as_option())
            .and_then(M::trip_context);

        let solver = optimise_for(
            owned
                .options
                .as_option()
                .map(|o| o.optimise_for)
                .unwrap_or_default(),
        );
        let runtime = M::runtime(context);

        let opts = MatchOptions::new()
            .with_runtime(runtime.clone())
            .with_solver(solver)
            .with_search_distance(owned.search_distance);

        let result = self
            .inner
            .snap(coordinates, opts)
            .map_err(|e| e.to_string())
            .map_err(ConnectError::internal)?;

        Ok(SnapResponse {
            matches: Util::<M::Runtime>::process::<E, M>(result, runtime),
            ..Default::default()
        }
        .into())
    }
}
