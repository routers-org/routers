use geo::{Coord, Distance, Geodesic, Point};
use std::marker::PhantomData;
use std::sync::Arc;
use tonic::{Request, Response, Status};

use crate::definition::r#match::*;
use crate::definition::model::*;

use crate::services::RouteService;
use routers::{Match, Path, RoutedPath};
use routers_codec::{Entry, Metadata};
#[cfg(feature = "telemetry")]
use tracing::Level;

struct Util<Ctx>(PhantomData<Ctx>);

impl<Ctx> Util<Ctx> {
    fn coordinate_from_point(point: Point) -> Coordinate {
        <geo::Point as Into<Coord>>::into(point).into()
    }

    fn route_from_path<E: Entry, M: Metadata>(input: Path<E, M>, ctx: &Ctx) -> Vec<RouteElement>
    where
        EdgeMetadata: for<'a> From<(&'a M, &'a Ctx)>,
    {
        input
            .iter()
            .flat_map(|entry| {
                let edge = EdgeBuilder::default()
                    .id(entry.edge.id().identifier())
                    .source(entry.edge.source)
                    .target(entry.edge.target)
                    .metadata(EdgeMetadata::from((&entry.metadata, ctx)))
                    .length(
                        Geodesic.distance(entry.edge.source.position, entry.edge.target.position),
                    )
                    .build()
                    .unwrap();

                RouteElementBuilder::default()
                    .coordinate(Util::<Ctx>::coordinate_from_point(entry.point))
                    .edge(RouteEdge {
                        edge: Some(edge),
                        ..RouteEdge::default()
                    })
                    .build()
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
        };

        vec![matched_route]
    }
}

#[tonic::async_trait]
impl<E, M> MatchService for RouteService<E, M>
where
    M: Metadata + 'static,
    E: Entry + 'static,
    EdgeMetadata: for<'a> From<(&'a M, &'a M::Runtime)>,
    Option<M::TripContext>: From<CostOptions>,
{
    #[cfg_attr(feature="telemetry", tracing::instrument(skip_all, level = Level::INFO))]
    async fn r#match(
        self: Arc<Self>,
        request: Request<MatchRequest>,
    ) -> Result<Response<MatchResponse>, Status> {
        let (.., message) = request.into_parts();
        let coordinates = message.linestring();

        // Find which solver to use...
        let solver = OptimiseFor::from(message.options);
        let runtime = M::runtime(message.trip_context::<M>());

        let result = self
            .graph
            .r#match(&runtime, solver, coordinates)
            .map_err(|e| e.to_string())
            .map_err(Status::internal)?;

        Ok(Response::new(MatchResponse {
            matches: Util::<M::Runtime>::process(result, runtime),
        }))
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

        let result = self
            .graph
            .snap(&runtime, solver, coordinates)
            .map_err(|e| e.to_string())
            .map_err(Status::internal)?;

        Ok(Response::new(SnapResponse {
            matches: Util::<M::Runtime>::process(result, runtime),
        }))
    }
}
